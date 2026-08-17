//! The read-only Diff view: one file's hunks as real syntax-highlighted rows, its gutter,
//! its fold markers, and the highlight cache that keeps re-rendering it cheap.

use super::zoom::zoom_scoped;
use super::*;
#[cfg(test)]
use crate::code_surface::fixtures::temp_repo;
use crate::review_notes::{NoteAnchor, NoteMark};
use crate::root::plural;
use crate::root::scrollbar;
use crate::root::widgets::render_sidebar_message;
#[cfg(test)]
use crate::test_support::open_test_app;
use std::rc::Rc;

/// Every row of the Diff view's virtualized list, in the flat order it scrolls in - built once
/// per frame by [`diff_rows`] and then indexed by `uniform_list`'s row builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffRow {
    /// `⋯ N unchanged lines` between two hunks - `gap` from `changes::fold_gap_between`, never
    /// an estimate (see [`render_fold_marker`]). `before_hunk` is the index of the hunk this
    /// marker sits directly above; it names nothing about the content, only this row's own
    /// `diff-fold-marker-{before_hunk}` debug selector, which needs to be stable and unique
    /// across a file with several gaps for a real render test to measure one specific marker.
    FoldMarker { gap: usize, before_hunk: usize },
    /// One hunk's own `@@ ... @@` header, by index into `DiffFile::hunks`.
    HunkHeader(usize),
    /// One real diff line. `hunk`/`line` index `DiffFile::hunks` (and, through the identity
    /// guard, the index-aligned [`AdeApp::diff_highlight_cache`]); `row` is the flat
    /// rendered-line counter that names this row's `diff-line-{row}` debug selector and that
    /// [`MAX_RENDERED_DIFF_LINES_PER_FILE`] caps - all three are kept because the cache is
    /// per-hunk while the cap and the selector are per-file.
    Line {
        hunk: usize,
        line: usize,
        row: usize,
    },
    /// One pinned review note (GitHub issue #288), directly beneath the diff line it is anchored
    /// to.
    Note { anchor: NoteAnchor, note_row: usize },
    /// The trailing `... diff truncated for this file` notice - a genuine final *item* of the
    /// list, not a sibling below it (see [`render_diff_truncated_row`]).
    Truncated,
}

/// The Diff view's whole row plan for `file`, in scroll order: fold markers between hunks that
/// have a real unchanged gap, each hunk's header, that hunk's lines up to
/// [`MAX_RENDERED_DIFF_LINES_PER_FILE`], and a trailing truncation notice when this file's diff
/// really was cut short (either by that cap here or by `wt_core::diff`'s own load-time cap,
/// `DiffFile::truncated`).
fn diff_rows(file: &DiffFile, noted: &[NoteAnchor]) -> Vec<DiffRow> {
    // At most one header + one fold marker per hunk, the capped line count, the notice, and one
    // row per real note.
    let mut rows: Vec<DiffRow> = Vec::with_capacity(
        file.hunks.len() * 2 + MAX_RENDERED_DIFF_LINES_PER_FILE + 1 + noted.len(),
    );
    let mut rendered_lines = 0usize;
    let mut note_rows = 0usize;
    let mut hunks_truncated = false;
    let mut previous_header: Option<&str> = None;
    'hunks: for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        if let Some(previous) = previous_header {
            if let Some(gap) = changes::fold_gap_between(previous, &hunk.header) {
                rows.push(DiffRow::FoldMarker {
                    gap,
                    before_hunk: hunk_index,
                });
            }
        }
        previous_header = Some(hunk.header.as_str());
        rows.push(DiffRow::HunkHeader(hunk_index));

        // Derived here rather than read off `AdeApp::diff_highlight_cache`'s index-aligned copy:
        // this function is pure and directly testable without a window, and that is the whole
        // reason the row plan lives apart from the render method.
        let numbers = if noted.is_empty() {
            Vec::new()
        } else {
            changes::hunk_line_numbers(hunk)
        };

        for line_index in 0..hunk.lines.len() {
            if rendered_lines >= MAX_RENDERED_DIFF_LINES_PER_FILE {
                hunks_truncated = true;
                break 'hunks;
            }
            rows.push(DiffRow::Line {
                hunk: hunk_index,
                line: line_index,
                row: rendered_lines,
            });
            rendered_lines += 1;
            let anchor = numbers
                .get(line_index)
                .copied()
                .and_then(NoteAnchor::from_gutter);
            if let Some(anchor) = anchor.filter(|anchor| noted.contains(anchor)) {
                rows.push(DiffRow::Note {
                    anchor,
                    note_row: note_rows,
                });
                note_rows += 1;
            }
        }
    }

    if file.truncated || hunks_truncated {
        rows.push(DiffRow::Truncated);
    }
    rows
}

/// Every diff line of `file` that a review note could be pinned to, in the order the diff really
/// lays them out, top to bottom.
pub(crate) fn note_anchors_in_diff_order(file: &DiffFile) -> Vec<NoteAnchor> {
    let mut anchors = Vec::new();
    let mut rendered_lines = 0usize;
    'hunks: for hunk in &file.hunks {
        for numbers in changes::hunk_line_numbers(hunk) {
            if rendered_lines >= MAX_RENDERED_DIFF_LINES_PER_FILE {
                break 'hunks;
            }
            rendered_lines += 1;
            if let Some(anchor) = NoteAnchor::from_gutter(numbers) {
                anchors.push(anchor);
            }
        }
    }
    anchors
}

/// Which surface [`AdeApp::render_diff_file_detail`] is drawing into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffDetailSurface {
    /// The git-side Diff view, opened from the Changes sidebar or the File/Diff toggle.
    Changes,
    /// The agent Review tab (`crate::review::render`).
    Review,
}

impl DiffDetailSurface {
    /// This surface's element-id and `debug_selector` prefix - `diff-line-3` vs. `review-line-3`.
    /// Two surfaces rendering rows under one id prefix would produce duplicate GPUI element ids
    /// whenever both were mounted, and would make a render test unable to say which surface it
    /// actually measured.
    pub(crate) fn id_prefix(self) -> &'static str {
        match self {
            DiffDetailSurface::Changes => "diff",
            DiffDetailSurface::Review => "review",
        }
    }

    fn scrollbar_id(self) -> &'static str {
        match self {
            DiffDetailSurface::Changes => "diff-view-scrollbar",
            DiffDetailSurface::Review => "review-view-scrollbar",
        }
    }

    fn scroll_handle(self, app: &AdeApp) -> &gpui::UniformListScrollHandle {
        match self {
            DiffDetailSurface::Changes => &app.diff_view_scroll_handle,
            DiffDetailSurface::Review => &app.review_scroll_handle,
        }
    }

    fn highlight_cache(self, app: &AdeApp) -> &Option<DiffHighlightCache> {
        match self {
            DiffDetailSurface::Changes => &app.diff_highlight_cache,
            DiffDetailSurface::Review => &app.review_highlight_cache,
        }
    }

    /// The `DiffFile` this surface currently has open, if any.
    fn open_file(self, app: &AdeApp) -> Option<&DiffFile> {
        match self {
            DiffDetailSurface::Changes => app.open_diff_file_cache.as_ref(),
            DiffDetailSurface::Review => app.open_review_file_detail(),
        }
    }
}

impl AdeApp {
    /// Ensures [`Self::diff_highlight_cache`] holds real per-hunk syntax highlighting for
    /// [`Self::open_diff_file_cache`] - recomputes (via [`code_view::highlight_block`]) only
    /// when the open file differs from what's cached (a cheap struct-equality check). Called
    /// only from [`Self::refresh_open_diff_file_cache`] - the real point `open_diff_file_cache`
    /// itself changes (a genuine action/event handler, e.g. `Self::open_change_diff`, never a
    /// render method) - **never** from `render()`: [`Self::render_diff_file_detail`] only reads
    /// this cache, so a still-recomputing cache can never block a render call the way calling
    /// this from inside it used to. Applied here as a synchronous call at the real change point,
    /// not a background `cx.spawn()` task like [`Self::spawn_file_load`] uses for a whole file's
    /// `load_file` - justified below by the real, measured cost this cap keeps small, not
    /// assumed.
    pub(crate) fn ensure_diff_highlight_cache(&mut self) {
        let Some(file) = self.open_diff_file_cache.clone() else {
            self.diff_highlight_cache = None;
            return;
        };
        if self
            .diff_highlight_cache
            .as_ref()
            .is_some_and(|(cached, _, _)| cached == &file)
        {
            return;
        }
        let extension = file.path.extension().and_then(|ext| ext.to_str());
        let highlight_options = self.highlight_options();
        let mut remaining = MAX_RENDERED_DIFF_LINES_PER_FILE;
        let mut per_hunk = Vec::with_capacity(file.hunks.len());
        let mut per_hunk_numbers = Vec::with_capacity(file.hunks.len());
        for hunk in &file.hunks {
            if remaining == 0 {
                break;
            }
            let capped_lines: Vec<&str> = hunk
                .lines
                .iter()
                .take(remaining)
                .map(|line| line.content.as_str())
                .collect();
            remaining -= capped_lines.len();
            per_hunk.push(code_view::highlight_block(
                capped_lines,
                extension,
                highlight_options,
            ));
            // Computed once here, alongside the highlighting it's index-aligned with, rather
            // than fresh inside `render_diff_file_detail`'s per-render loop (a real per-frame
            // `Vec` reallocation for every hunk that loop used to pay unconditionally).
            per_hunk_numbers.push(changes::hunk_line_numbers(hunk));
        }
        self.diff_highlight_cache = Some((file, per_hunk, per_hunk_numbers));
    }

    /// One changed file's diff content: a "binary file" note, or its hunks as unified-diff-style
    /// lines - real per-token syntax coloring and a real two-column old/new line-number gutter,
    /// both a pure read of [`Self::diff_highlight_cache`] (kept fresh by
    /// [`Self::ensure_diff_highlight_cache`]), with diff-kind coloring expressed only via row
    /// background tint + a left-edge accent bar + sign glyph (never the line text itself) so it
    /// doesn't fight syntax coloring for the same tokens - and a
    /// `⋯ N unchanged lines` fold marker for the gap between consecutive hunks
    /// (`crate::sidebar::changes::fold_gap_between`, parsed from the hunks' `@@ ... @@` headers).
    /// `wt_core::diff` has no lazy per-file hunk-loading state, since every non-binary changed
    /// file's hunks are already eagerly loaded, so the design's "press ⏎ to load this hunk"
    /// treatment doesn't apply here; capped by [`MAX_RENDERED_DIFF_LINES_PER_FILE`] independent
    /// of `wt_core::diff`'s own load-time cap.
    pub(crate) fn render_diff_file_detail(
        &self,
        file: &DiffFile,
        surface: DiffDetailSurface,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rem_px = self.effective_code_rem_px();

        if file.is_binary {
            return zoom_scoped(
                rem_px,
                render_diff_message_pane(
                    format!("{}-detail-binary", surface.id_prefix()),
                    "binary file (contents not diffed)".to_string(),
                ),
            );
        }

        // A rename-only file produces zero `@@` hunks, so falling through to the list below would
        // leave it with no items at all - a blank pane that looks like a rendering bug rather
        // than "nothing to show". `changes::empty_hunks_message` picks honest wording, naming the
        // rename specifically when that's the cause.
        if file.hunks.is_empty() {
            return zoom_scoped(
                rem_px,
                render_diff_message_pane(
                    format!("{}-detail-empty", surface.id_prefix()),
                    changes::empty_hunks_message(file.status).to_string(),
                ),
            );
        }

        // Resolved once per frame, here, and shared with the row builder through an `Rc` - the
        // same indirection (and the same reason) as `crate::sidebar::render::AdeApp::
        // render_file_tree`'s own row list: `uniform_list` invokes its closure several times per
        // frame (once at `0..1` to measure the row height, again during prepaint, then for the
        // real visible range), so re-deriving this plan inside it would walk every hunk of the
        // file that many times per frame - most of the cost this change exists to remove.
        // It also has to exist before the list does, since `uniform_list` needs a real item count
        // up front, and both must come from the same plan.
        // GitHub issue #288. Resolved once per frame, like `rows` itself and for the same reason,
        // and empty on any surface but the Uncommitted diff - see
        // `crate::review_notes::flow::AdeApp::review_notes_file` for why notes are scoped to that
        // one surface rather than to every place this renderer is used.
        let notes_path: Option<PathBuf> = match surface {
            DiffDetailSurface::Changes => {
                self.review_notes_file().filter(|path| path == &file.path)
            }
            DiffDetailSurface::Review => None,
        };
        let noted: Vec<NoteAnchor> = notes_path
            .as_ref()
            .map(|path| {
                self.review_notes_store()
                    .anchors(&self.review_notes_worktree(), path)
            })
            .unwrap_or_default();
        let notes_enabled = notes_path.is_some();
        // Each line's note anchor, resolved once per frame from the same
        // `changes::hunk_line_numbers` the row plan itself uses - deliberately **not** from
        // `diff_highlight_cache`. A line's identity is a fact about the diff, not about whether
        // its syntax colouring has been recomputed yet: reading it off the cache would mean that
        // every time the agent writes to the open file (invalidating the cache's full-value
        // identity guard until the next highlight pass) every note dot vanished and no line could
        // be clicked, with the already-pinned cards still on screen and no explanation.
        let line_anchors: Rc<Vec<Vec<Option<NoteAnchor>>>> = Rc::new(if notes_enabled {
            file.hunks
                .iter()
                .map(|hunk| {
                    changes::hunk_line_numbers(hunk)
                        .into_iter()
                        .map(NoteAnchor::from_gutter)
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        });
        // The bar is built after the list, from the same resolution - one `Rc`, so the row builder
        // and the bar can never be looking at two different files.
        let notes_path: Option<Rc<PathBuf>> = notes_path.map(Rc::new);
        let bar_path = notes_path.clone();

        let rows: Rc<Vec<DiffRow>> = Rc::new(diff_rows(file, &noted));
        let row_count = rows.len();

        // GitHub issue #287's gutter attribution, resolved once per frame for the same reason
        // `rows` is: the row builder runs several times per frame, and this walks every hunk.
        // Empty whenever this worktree has nothing on record for this path, which is what makes
        // "attribution renders only where provenance exists" structural rather than a rule each
        // row has to remember - there is simply no author to read.
        let line_authors: Rc<Vec<Vec<crate::provenance::Author>>> =
            Rc::new(self.diff_line_authors(file));
        // The filter is a fact about the file the *Changes* surface has open
        // (`AdeApp::active_author_filter` checks it against `open_change`), so the Review tab -
        // which can hold a different file at the same time - is never dimmed by it.
        let author_filter: Option<crate::provenance::Author> = match surface {
            DiffDetailSurface::Changes => self.active_author_filter().cloned(),
            DiffDetailSurface::Review => None,
        };

        let list = uniform_list(
            // Per-surface and per-path (see `DiffDetailSurface::id_prefix`'s own docs): a
            // different open diff, or the other surface entirely, is a different list, not the
            // same one showing new content.
            format!("{}-detail-{}", surface.id_prefix(), file.path.display()),
            row_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                // Re-resolved from `this` through `surface` rather than a hardcoded field,
                // mirroring `crate::graph_view::render::AdeApp::render_graph_rows`' identical
                // re-resolve: the closure is `'static` and cannot borrow the `&DiffFile` this
                // method was handed, and cloning one (up to `wt_core::diff`'s own per-file line
                // cap) per frame would be a real cost of its own. `Self::render_center_pane`/
                // `crate::review::render::AdeApp::render_review_view` each take their own surface's
                // open-file field out only for the duration of their own `render()` call and put
                // it straight back, so it is genuinely populated by the time this closure runs
                // (layout), the same lifecycle `render_graph_rows` relies on.
                let Some(file) = surface.open_file(this) else {
                    return Vec::new();
                };
                // Guarded exactly like the highlight cache below: this frame's notes belong to
                // the file the plan was built from, and if the surface has since resolved a
                // *different* file, drawing them would put file A's notes on file B's rows - and
                // a click would then write a note under A's path against B's line.
                let notes_path = notes_path
                    .as_ref()
                    .filter(|open| open.as_path() == file.path);
                // The real identity guard - a cache entry only counts as usable for these rows if
                // it was built from this exact file (see this method's own "Cache identity guard"
                // docs, and `diff_highlight_cache_for`'s own docs/tests for the pure logic).
                let cache = diff_highlight_cache_for(surface.highlight_cache(this), file);
                // Clamped rather than trusted, and `start` against `end` rather than only against
                // the length, so a divergence degrades to "renders fewer rows" instead of
                // panicking on an inverted range.
                let end = range.end.min(rows.len());
                let start = range.start.min(end);
                rows[start..end]
                    .iter()
                    .map(|row| match *row {
                        DiffRow::FoldMarker { gap, before_hunk } => {
                            render_fold_marker(gap, before_hunk).into_any_element()
                        }
                        DiffRow::HunkHeader(index) => match file.hunks.get(index) {
                            Some(hunk) => {
                                render_hunk_header(&hunk.header, index).into_any_element()
                            }
                            // Only reachable if the open file changed shape between this frame's
                            // row plan and this call; an empty row of the shared height keeps the
                            // list's geometry honest instead of panicking.
                            None => render_blank_diff_row().into_any_element(),
                        },
                        DiffRow::Line { hunk, line, row } => {
                            match file.hunks.get(hunk).and_then(|h| h.lines.get(line)) {
                                Some(diff_line) => {
                                    let rendered = cache
                                        .and_then(|(per_hunk, _)| per_hunk.get(hunk))
                                        .and_then(|lines| lines.get(line));
                                    let numbers = cache
                                        .and_then(|(_, per_hunk_numbers)| {
                                            per_hunk_numbers.get(hunk)
                                        })
                                        .and_then(|nums| nums.get(line))
                                        .copied()
                                        .unwrap_or((None, None));
                                    // Positional, and guarded exactly like the highlight cache
                                    // beside it: if the open file changed shape between this
                                    // frame's plan and this call, the answer is "no author",
                                    // never a neighbouring line's author.
                                    let author = line_authors
                                        .get(hunk)
                                        .and_then(|authors| authors.get(line));
                                    // Positional, and guarded exactly like the author bar beside
                                    // it: a shape change between this frame's plan and this call
                                    // means "not annotatable", never a neighbouring line's
                                    // anchor.
                                    let anchor = line_anchors
                                        .get(hunk)
                                        .and_then(|anchors| anchors.get(line))
                                        .copied()
                                        .flatten();
                                    let note = anchor.zip(notes_path).and_then(|(anchor, path)| {
                                        this.review_notes_store()
                                            .note(
                                                &this.review_notes_worktree(),
                                                path.as_path(),
                                                anchor,
                                            )
                                            .map(|note| note.mark())
                                    });
                                    let chrome = DiffLineChrome {
                                        rendered,
                                        numbers,
                                        selector_prefix: surface.id_prefix(),
                                        row_index: row,
                                        author,
                                        filter: author_filter.as_ref(),
                                        anchor,
                                        note,
                                        notes_enabled,
                                        is_note_cursor: anchor.is_some()
                                            && this.note_cursor.as_ref().is_some_and(|cursor| {
                                                notes_path.is_some_and(|path| cursor.path == **path)
                                                    && Some(cursor.anchor) == anchor
                                            }),
                                    };
                                    render_diff_line(diff_line, chrome, cx).into_any_element()
                                }
                                None => render_blank_diff_row().into_any_element(),
                            }
                        }
                        DiffRow::Note { anchor, note_row } => match notes_path {
                            Some(path) => this
                                .render_review_note_card(
                                    path.as_ref().clone(),
                                    anchor,
                                    surface.id_prefix(),
                                    note_row,
                                    cx,
                                )
                                .into_any_element(),
                            // Only reachable if the notes surface changed under this frame's own
                            // row plan; an empty row of the shared height keeps the list's
                            // geometry honest instead of drawing a note for a file it is not on.
                            None => render_blank_diff_row().into_any_element(),
                        },
                        DiffRow::Truncated => render_diff_truncated_row().into_any_element(),
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .bg(theme::surface::PTY)
        // GitHub issue #30's real overlay scrollbar reads its geometry straight off this same
        // handle (`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`).
        .track_scroll(surface.scroll_handle(self));

        let hunks = zoom_scoped(rem_px, self.wrap_with_scrollbar(surface, list, cx));
        match bar_path {
            // GitHub issue #288: *"A notes bar above the hunks"* - the audit's "send from the top
            // of the diff", which is one of the three things Orca is quoted as having learned the
            // hard way. Deliberately **outside** `zoom_scoped`: the bar is chrome, not code, and
            // zooming the diff's text has no business resizing a button.
            Some(path) => self.wrap_diff_with_notes(path.as_ref().clone(), hunks, cx),
            None => hunks,
        }
    }

    /// Wraps the Diff view's `uniform_list` in the real, non-scrolling `.relative()` sibling
    /// wrapper GitHub issue #30's overlay scrollbar needs - see `crate::sidebar::render::AdeApp::
    /// render_file_tree`'s own docs on why the scrollbar must never be a child of the scrolling
    /// element itself.
    fn wrap_with_scrollbar(
        &self,
        surface: DiffDetailSurface,
        content: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(content)
            .children(scrollbar::render_vertical_scrollbar(
                surface.scrollbar_id(),
                surface.scroll_handle(self),
                &[],
                cx,
            ))
            .into_any_element()
    }
}

/// One of the Diff view's two non-diff return points (a binary file, or a file with no hunks at
/// all) as a real pane: the same `theme::surface::PTY` background and top padding the row list
/// sits on, with one [`render_sidebar_message`] on it. Deliberately not scrollable - see
/// [`AdeApp::wrap_with_scrollbar`]'s own docs.
fn render_diff_message_pane(id: String, message: String) -> impl IntoElement {
    div()
        .id(id)
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .bg(theme::surface::PTY)
        .py(px(4.0))
        .child(render_sidebar_message(message, theme::text::FAINT.into()))
}

/// One hunk's `@@ ... @@` header row.
pub(in crate::code_surface) fn render_hunk_header(
    header: &str,
    hunk_index: usize,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(rems(1.6))
        .font(font(theme::font::MONO))
        .text_size(rems(1.0))
        .line_height(rems(1.6))
        .px(px(8.0))
        .bg(theme::diff::HUNK_BG)
        .text_color(theme::diff::HUNK_FG)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(header.to_string())
        // A no-op outside test builds, like every other `debug_selector` in this crate - lets a
        // real render test measure one specific hunk header's painted bounds.
        .debug_selector(move || format!("diff-hunk-header-{hunk_index}"))
}

/// The `... diff truncated for this file` notice, shown when this file's diff really was cut
/// short - by [`MAX_RENDERED_DIFF_LINES_PER_FILE`] here, or by `wt_core::diff`'s own load-time
/// cap (`DiffFile::truncated`).
fn render_diff_truncated_row() -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .h(rems(1.6))
        .px(px(8.0))
        .font(font(theme::font::MONO))
        .text_size(rems(0.85))
        .text_color(theme::text::FAINT)
        .whitespace_nowrap()
        .overflow_hidden()
        .child("... diff truncated for this file")
        .debug_selector(|| "diff-truncated-row".to_string())
}

/// An empty row at the shared height, for the one case the row builder can't resolve: a row plan
/// index that no longer names real content because the open `DiffFile` changed between this
/// frame's plan and the builder's call. Real, honest blank space - never a guess at what the row
/// used to hold, and never a panic.
fn render_blank_diff_row() -> impl IntoElement {
    div().h(rems(1.6))
}

/// The diff view's `⋯ N unchanged lines` fold marker. `N` is derived from the hunks' `@@ ... @@`
/// headers (`crate::sidebar::changes::fold_gap_between`), never an estimate.
pub(in crate::code_surface) fn render_fold_marker(
    gap: usize,
    before_hunk: usize,
) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .h(rems(1.6))
        .debug_selector(move || format!("diff-fold-marker-{before_hunk}"))
        .bg(theme::diff::FOLD_BG)
        .font(font(theme::font::MONO))
        .text_size(rems(0.85))
        .text_color(theme::diff::FOLD_FG)
        .child(format!(
            "\u{22ef} {}",
            plural::count(gap, "unchanged line", None)
        ))
}

/// The Diff view's cache-identity guard, factored out of [`AdeApp::render_diff_file_detail`] as
/// its own pure function so it's directly unit-testable without a real GPUI window: `cache`
/// counts as usable for rendering `file` only if the `DiffFile` it was actually built from
/// (`cached`) equals `file` - the fix for a real, CRITICAL bug (see
/// `render_diff_file_detail`'s own "Cache identity guard" docs) where `hunk_index`/`line_index`
/// positions were read out of `cache` with no check that it belonged to the file on screen, so a
/// stale cache could silently render one file's real source lines under a *different* file's
/// correct diff signs and gutter numbers. Returns `None` - not the mismatched data - whenever
/// `cache` is empty or belongs to a different file, so every caller downstream (both
/// `per_hunk`/`per_hunk_numbers`, together) falls back to real, honest plain-text/blank-gutter
/// rendering instead. Covered directly by
/// `diff_render_tests::cache_identity_guard_rejects_a_mismatched_cache_entry` and
/// `diff_render_tests::cache_identity_guard_accepts_a_matching_cache_entry`.
type DiffHighlightCacheRef<'a> = (
    &'a Vec<Vec<code_view::RenderedLine>>,
    &'a Vec<Vec<(Option<usize>, Option<usize>)>>,
);

fn diff_highlight_cache_for<'a>(
    cache: &'a Option<DiffHighlightCache>,
    file: &DiffFile,
) -> Option<DiffHighlightCacheRef<'a>> {
    cache
        .as_ref()
        .filter(|(cached, _, _)| cached == file)
        .map(|(_, per_hunk, per_hunk_numbers)| (per_hunk, per_hunk_numbers))
}

/// One fixed-`px()` right-aligned diff-gutter number column (old or new line number) - blank for
/// `None`, matching the File view gutter's own zoom-safety precedent
/// (`render_file_view_line`'s docs): a real derived line number never grows with zoom, so it can
/// never wrap inside its fixed-width column.
fn render_diff_gutter_number(number: Option<usize>) -> impl IntoElement {
    div()
        .flex_none()
        // 44px, not the File view gutter's 52px: this column shows one *narrower* number (a
        // single old- or new-file line count, not both stacked) at a smaller `px(10.0)` text
        // size, but still real digits that must never wrap - a real 4-to-5-digit line
        // number (this crate's own largest file, `lsp/client.rs`, is already 3,618 real
        // lines) must fit without
        // wrapping into a second visual line, exactly the class of bug Revision R5's audit fixed
        // once already for the File view's own gutter (`render_file_view_line`'s docs). Real
        // width headroom, not just a wider number: `.whitespace_nowrap()`/`.overflow_hidden()`
        // below are the same real defensive backstop, so an even-longer number clips rather than
        // wrapping and growing this row's height past its neighbours'. That is no longer just
        // tidiness: since GitHub issue #224 this view *is* a `uniform_list`, which lays every row
        // out at the height it measured from item 0 alone, so a row that grew taller would be
        // silently clipped - the exact bug Revision R5's audit fixed for the File view's own
        // gutter (`render_file_view_line`'s docs), reachable here too now.
        .w(px(44.0))
        .pr(px(6.0))
        .text_right()
        .whitespace_nowrap()
        .overflow_hidden()
        .font(font(theme::font::MONO))
        .text_size(px(10.0))
        .text_color(theme::text::GUTTER)
        .child(number.map(|n| n.to_string()).unwrap_or_default())
}

/// One diff line: a real two-column old/new line-number gutter (`numbers`, precomputed by
/// [`AdeApp::ensure_diff_highlight_cache`] via `changes::hunk_line_numbers`), a 3px left-edge
/// accent bar + sign glyph colored by diff kind (`+`/`\u{2212}`/` `, plus the row's background
/// tint for Added/Removed), and the line's real text as per-token syntax-colored runs
/// (`rendered`, from [`AdeApp::diff_highlight_cache`]). Diff-kind coloring is deliberately
/// expressed only via the row background tint + accent bar + sign glyph, never the text itself,
/// so it doesn't fight the real syntax coloring for the same tokens - see this function's own
/// `accent` binding for why that's `ADD_FG`/`DEL_FG`, not `ADD_SIGN`/`DEL_SIGN`.
pub(in crate::code_surface) fn render_diff_line(
    line: &wt_core::diff::DiffLine,
    chrome: DiffLineChrome<'_>,
    cx: &mut Context<AdeApp>,
) -> impl IntoElement {
    let DiffLineChrome {
        rendered,
        numbers,
        selector_prefix,
        row_index,
        author,
        filter,
        anchor,
        note,
        is_note_cursor,
        notes_enabled,
    } = chrome;
    // `accent` (`Some` only for Added/Removed) drives both the left-edge bar below and the sign
    // glyph's own color - [`theme::diff::ADD_FG`]/[`DEL_FG`], not the more muted `ADD_SIGN`/
    // `DEL_SIGN` (still used elsewhere, for the Changes list's +n/-n stat bar - see
    // `changes::stat_segment_color`): now that real per-token syntax coloring owns the line
    // text, the only remaining add/remove signal was this sign glyph plus a subtle background
    // tint, and a real contrast check found `DEL_SIGN` against this surface's background
    // (`theme::surface::PTY`) sits under WCAG AA's 4.5:1 text threshold (~4.0:1) - `DEL_FG`/
    // `ADD_FG` (originally the pre-syntax-highlighting full-line text colors, otherwise dead
    // code since Revision R9a's highlighting change) measure comfortably above it (~8.8:1/
    // ~11.1:1), so reusing them here both fixes the contrast and strengthens the at-a-glance
    // add/remove signal the way a wider tint or a bare color bump alone wouldn't - a real
    // left-edge accent bar is closer to how `code_surface::file_view::render_file_view_line`'s own
    // git-gutter marker already flags a changed line for the File view, applied here too.
    let (sign, accent, bg) = match line.kind {
        DiffLineKind::Added => ("+", Some(theme::diff::ADD_FG), Some(theme::diff::ADD_BG)),
        DiffLineKind::Removed => (
            "\u{2212}",
            Some(theme::diff::DEL_FG),
            Some(theme::diff::DEL_BG),
        ),
        DiffLineKind::Context => (" ", None, None),
    };
    let sign_color = accent.unwrap_or(theme::diff::CTX_FG);

    // `Some` only for a line a real author is on record for and that this build can name - see
    // this function's own docs.
    let author_bar = author.and_then(crate::provenance::render::author_gutter_color);
    let dimmed = crate::provenance::render::line_is_dimmed(author, filter);

    let row_selector = format!("{selector_prefix}-line-{row_index}");
    let mut row = div()
        // Stateful since GitHub issue #288: the row is the note gesture's own hit target, and
        // `on_click`/`hover` both need a real element id. The id is the selector, so the two can
        // never name different rows.
        .id(gpui::SharedString::from(row_selector.clone()))
        .flex()
        // A row is at least as wide as the pane, and may be wider.
        //
        // Live report, found while reproducing GitHub issue #288's own: a click in the empty
        // space to the right of a short diff line did **nothing at all**. The row is the note
        // gesture's hit target (see the `on_click` at the bottom of this function), and a
        // `gpui::uniform_list` item whose own width is `auto` is laid out at its *content* width -
        // `Drawable::layout_as_root` never goes through the `stretch_auto_size_to_fill` the window
        // root does (see `crate::review_notes::render::AdeApp::render_review_note_card`'s own
        // note on the same mechanism). So the hit box, the hover lift and the add/remove tint all
        // stopped at the last glyph of the line, several hundred pixels short of the pane, with
        // nothing on screen saying where the clickable part ended.
        //
        // `min_w_full` rather than `w_full` deliberately: a line longer than the pane must still
        // be able to lay itself out at its real width (the list's own content width is measured
        // from these items), which a hard `width:100%` would forbid.
        .min_w_full()
        .items_center()
        .font(font(theme::font::MONO))
        .text_size(rems(1.0))
        .line_height(rems(1.6))
        // `debug_selector` is a no-op outside test builds; lets a real render test measure this
        // row's painted bounds and confirm the diff view's own rows are genuinely reachable, the
        // same pattern `render_file_view_line`'s `file-view-text-row-{n}` selector already
        // establishes for the File view.
        .debug_selector(move || row_selector);
    if let Some(bg) = bg {
        row = row.bg(bg);
    }
    if dimmed {
        row = row.opacity(crate::provenance::render::FILTER_DIM_OPACITY);
    }
    // The author channel, ahead of the diff-kind accent below it - see this function's own docs.
    // The box is only painted when there is a real author to paint: an unattributed line's gutter
    // is genuinely empty, and the 2px it would have occupied belongs to nothing.
    if let Some(bar) = author_bar {
        let tooltip = author.and_then(crate::provenance::render::author_tooltip);
        let bar_selector = format!("{selector_prefix}-author-{row_index}");
        row = row.child(
            div()
                .id(gpui::SharedString::from(bar_selector.clone()))
                .debug_selector(move || bar_selector)
                .flex_none()
                .w(px(2.0))
                .self_stretch()
                .bg(bar)
                .when_some(tooltip, |el, tip| {
                    el.tooltip(crate::root::widgets::text_tooltip(tip))
                }),
        );
    }
    let kind_selector = format!("{selector_prefix}-kind-{row_index}");
    row = row.child(
        div()
            // Named so a render test can prove the two gutter channels really coexist: GitHub
            // issue #287's author bar is an *additional* channel, and a version of it that had
            // quietly replaced this one would still look plausible in a screenshot.
            .debug_selector(move || kind_selector)
            .flex_none()
            .w(px(3.0))
            // `self_stretch()`, not a fixed height - matches `render_file_view_line`'s own
            // git-gutter marker so consecutive added/removed rows read as one continuous strip
            // rather than leaving gaps at higher zoom.
            .self_stretch()
            .bg(accent.unwrap_or(theme::ColorToken::literal(work_surface::TRANSPARENT))),
    );

    let mut text_row = div().flex().flex_1().min_w_0();
    match rendered {
        Some(rendered_line) if !rendered_line.text.is_empty() => {
            for (run_text, kind) in &rendered_line.runs {
                text_row = text_row.child(
                    div()
                        .text_color(code_view::color_for_kind(*kind))
                        .child(run_text.clone()),
                );
            }
        }
        Some(_) => text_row = text_row.child("\u{a0}"),
        None => {
            text_row = text_row.child(div().text_color(theme::syntax::TEXT).child(
                if line.content.is_empty() {
                    "\u{a0}".to_string()
                } else {
                    line.content.clone()
                },
            ))
        }
    }

    row.children(notes_enabled.then(|| render_note_column(note, is_note_cursor)))
        .child(render_diff_gutter_number(numbers.0))
        .child(render_diff_gutter_number(numbers.1))
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .text_center()
                .text_size(px(11.0))
                .text_color(sign_color)
                .child(sign),
        )
        .child(text_row)
        // The gesture itself, and only where a line really has an identity to pin a note to.
        // The gesture itself, and only where a line really has an identity to pin a note to.
        //
        // Deliberately **no tooltip on the row**. Ride-along I11 asks for one on *"every icon-only
        // or otherwise unlabelled control"*, and a diff line is neither - it is content, and the
        // note column beside it is the affordance. A row-wide tooltip also turned out to be a real
        // interaction hazard rather than only noise: it is a deferred overlay drawn next to the
        // pointer, i.e. potentially directly under it, where it can absorb the very click it is
        // describing. The card and the send button, which *are* controls, keep theirs.
        .when_some(anchor, |el, anchor| {
            el.cursor_pointer()
                .hover(|style| style.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.toggle_line_note(anchor, window, cx);
                }))
        })
}

/// `Jerry.dc.html`'s own 14px note column: `●` for a pinned note, `○` for the note cursor, and
/// genuinely nothing for a line that is neither.
fn render_note_column(note: Option<NoteMark>, is_note_cursor: bool) -> impl IntoElement {
    let (glyph, color) = match (note, is_note_cursor) {
        (Some(_), _) => ("\u{25cf}", theme::notes::DOT),
        (None, true) => ("\u{25cb}", theme::notes::DOT_EMPTY),
        (None, false) => ("", theme::notes::DOT_EMPTY),
    };
    div()
        .flex_none()
        .w(px(14.0))
        .text_center()
        .text_size(px(9.0))
        .text_color(color)
        .child(glyph)
}

/// Everything [`render_diff_line`] needs beyond the diff line itself.
pub(in crate::code_surface) struct DiffLineChrome<'a> {
    /// This line's per-token syntax highlighting, or `None` when the cache identity guard could
    /// not confirm the cache belongs to this file.
    pub rendered: Option<&'a code_view::RenderedLine>,
    /// `(old, new)` gutter numbers, same source and same guard.
    pub numbers: (Option<usize>, Option<usize>),
    /// `diff` or `review` - see [`DiffDetailSurface::id_prefix`].
    pub selector_prefix: &'static str,
    /// The flat rendered-line counter this row's selector is expressed in.
    pub row_index: usize,
    /// Who wrote this line (GitHub issue #287).
    pub author: Option<&'a crate::provenance::Author>,
    /// The per-author filter in force, if any.
    pub filter: Option<&'a crate::provenance::Author>,
    /// This line's review-note anchor, or `None` for a line with no stable file-line identity -
    /// which is also what makes the row un-clickable (GitHub issue #288).
    pub anchor: Option<NoteAnchor>,
    /// The mark of the note pinned here, if there is one.
    pub note: Option<NoteMark>,
    /// Whether this is the line `C` would toggle a note on.
    pub is_note_cursor: bool,
    /// Whether this surface takes review notes at all. Gates the 14px note column outright, so
    /// the Review tab - where a note can never exist - carries no permanently-blank column, while
    /// the Uncommitted diff keeps the column on *every* row so pinning a note never shifts the
    /// lines around it sideways.
    pub notes_enabled: bool,
}

/// Real, render-level coverage for the Diff view's per-token syntax highlighting and its
/// caching (`AdeApp::diff_highlight_cache`/`ensure_diff_highlight_cache`) - `render_diff_line`'s
/// entire output shape changed in Revision R9a and, until this module existed, not one test
/// actually rendered a real diff and checked anything about it.
#[cfg(test)]
mod diff_render_tests {
    use super::*;
    use gpui::TestAppContext;
    use test_support::git;

    #[gpui::test]
    fn opening_a_real_diff_renders_real_syntax_highlighted_rows(cx: &mut TestAppContext) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(
            repo.path().join("sample.rs"),
            "fn add(x: i32) -> i32 {\n    x + 1\n}\n",
        )
        .expect("write sample.rs");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(
            repo.path().join("sample.rs"),
            "fn add(x: i32) -> i32 {\n    x + 2\n}\n",
        )
        .expect("rewrite sample.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("sample.rs"), window, cx);
        });
        cx.run_until_parked();

        // Real structural check: this specific one-line change produces a real 4-line hunk
        // (unchanged "fn add..." context, removed "x + 1", added "x + 2", unchanged "}"
        // context) - every one of those rows must have really painted.
        for row_index in 0..4 {
            cx.debug_bounds(match row_index {
                0 => "diff-line-0",
                1 => "diff-line-1",
                2 => "diff-line-2",
                _ => "diff-line-3",
            })
            .unwrap_or_else(|| panic!("diff-line-{row_index} should have really painted"));
        }

        // Real content check: the cache `render_diff_file_detail`/`render_diff_line` actually
        // read from must hold real, non-flat per-token classification, not just plain text -
        // the `fn` keyword and the real changed integer literals.
        app.read_with(cx, |app, _| {
            let (cached_file, per_hunk, _) = app
                .diff_highlight_cache
                .as_ref()
                .expect("diff_highlight_cache should be populated after opening a real diff");
            assert_eq!(cached_file.path, PathBuf::from("sample.rs"));
            let all_runs: Vec<_> = per_hunk
                .iter()
                .flat_map(|lines| lines.iter())
                .flat_map(|line| line.runs.iter())
                .collect();
            assert!(
                all_runs.iter().any(|(text, kind)| text.as_ref() == "fn"
                    && *kind == code_view::HighlightKind::Keyword),
                "the real 'fn' keyword should be classified as a Keyword in the cache the \
                 render path reads from - got {all_runs:?}"
            );
            assert!(
                all_runs.iter().any(|(text, kind)| text.as_ref() == "2"
                    && *kind == code_view::HighlightKind::ConstantBuiltin),
                "the real added integer literal '2' should be classified as ConstantBuiltin (a \
                 real `@constant.builtin` capture - `tree-sitter-rust` has no separate `number` \
                 capture) - got {all_runs:?}"
            );
        });
    }

    #[gpui::test]
    fn repeated_refreshes_of_the_same_open_diff_reuse_the_cached_highlighting(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(
            repo.path().join("sample.rs"),
            "fn add() -> i32 {\n    1\n}\n",
        )
        .expect("write sample.rs");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(
            repo.path().join("sample.rs"),
            "fn add() -> i32 {\n    2\n}\n",
        )
        .expect("rewrite sample.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("sample.rs"), window, cx);
        });
        cx.run_until_parked();

        let first_ptr = app.read_with(cx, |app, _| {
            app.diff_highlight_cache
                .as_ref()
                .expect("diff_highlight_cache should be populated after opening a real diff")
                .1
                .as_ptr()
        });

        app.update(cx, |app, _cx| {
            app.refresh_open_diff_file_cache();
        });
        let second_ptr = app.read_with(cx, |app, _| {
            app.diff_highlight_cache
                .as_ref()
                .expect("diff_highlight_cache should still be populated")
                .1
                .as_ptr()
        });
        assert_eq!(
            first_ptr, second_ptr,
            "a second refresh of the same, unchanged open diff must reuse the cached \
             highlighting, not rebuild it (a fresh heap allocation means highlight_block ran \
             again for content that hadn't changed)"
        );
    }

    #[gpui::test]
    fn switching_the_open_diff_to_a_different_file_recomputes_the_highlight_cache(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(repo.path().join("a.rs"), "fn a() -> i32 {\n    1\n}\n")
            .expect("write a.rs");
        std::fs::write(repo.path().join("b.py"), "def b():\n    return 1\n").expect("write b.py");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.rs"), "fn a() -> i32 {\n    2\n}\n")
            .expect("rewrite a.rs");
        std::fs::write(repo.path().join("b.py"), "def b():\n    return 2\n").expect("rewrite b.py");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("a.rs"), window, cx);
        });
        cx.run_until_parked();
        let a_cached_path = app.read_with(cx, |app, _| {
            app.diff_highlight_cache
                .as_ref()
                .expect("diff_highlight_cache should be populated for a.rs")
                .0
                .path
                .clone()
        });
        assert_eq!(a_cached_path, PathBuf::from("a.rs"));

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("b.py"), window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let (cached_file, per_hunk, _) = app
                .diff_highlight_cache
                .as_ref()
                .expect("diff_highlight_cache should be populated for b.py");
            assert_eq!(
                cached_file.path,
                PathBuf::from("b.py"),
                "switching the open diff to a different real file must recompute the cache for \
                 that file, not keep serving a.rs's stale highlighting"
            );
            let has_python_keyword = per_hunk
                .iter()
                .flat_map(|lines| lines.iter())
                .flat_map(|line| line.runs.iter())
                .any(|(text, kind)| {
                    text.as_ref() == "def" && *kind == code_view::HighlightKind::Keyword
                });
            assert!(
                has_python_keyword,
                "b.py's real Python content should be highlighted with its own real grammar, \
                 not a.rs's Rust one"
            );
        });
    }

    #[gpui::test]
    fn a_diff_past_the_rendered_line_cap_still_highlights_every_line_it_actually_renders(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        test_support::seed_empty_repo_at(repo.path());
        std::fs::write(repo.path().join("big.rs"), "fn noop() {}\n").expect("write big.rs");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        let mut content = String::from("fn noop() {}\n");
        for index in 0..350 {
            content.push_str(&format!("fn generated_{index}() -> i32 {{ {index} }}\n"));
        }
        std::fs::write(repo.path().join("big.rs"), &content).expect("rewrite big.rs");

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("big.rs"), window, cx);
        });
        cx.run_until_parked();

        // Every real row up to the cap must have painted with a real `fn` keyword run - proving
        // the cache, not a `None`-fallback plain-text row, is what actually rendered it.
        app.read_with(cx, |app, _| {
            let (_, per_hunk, _) = app
                .diff_highlight_cache
                .as_ref()
                .expect("diff_highlight_cache should be populated");
            let total_rendered: usize = per_hunk.iter().map(|lines| lines.len()).sum();
            assert_eq!(
                total_rendered, MAX_RENDERED_DIFF_LINES_PER_FILE,
                "the cache must be truncated to exactly the real render cap, not the file's \
                 full, uncapped line count"
            );
        });

        let first_row = cx
            .debug_bounds("diff-line-0")
            .expect("the first diff row must really paint");
        // A deliberately huge delta: `uniform_list` clamps to its own real maximum scroll offset,
        // so this lands at the true bottom without this test having to model row heights or the
        // viewport itself (the same technique `crate::graph_view::render::
        // graph_virtualization_tests` uses).
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: first_row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("diff-line-299").is_some(),
            "the last row within the real render cap must really paint once it is scrolled to"
        );
        assert!(
            cx.debug_bounds("diff-line-300").is_none(),
            "a row past the real render cap must not exist at all - not even at the very bottom \
             of the list, which is exactly where it would be if the cap had been raised"
        );
    }

    /// A minimal but real `DiffFile` - one hunk, one context line - used by both cache-identity
    /// tests below so their only difference is the thing actually under test (the file the
    /// cache/lookup are keyed on), not incidental shape differences.
    fn sample_diff_file(path: &str) -> DiffFile {
        DiffFile {
            path: PathBuf::from(path),
            old_path: None,
            status: wt_core::diff::FileChangeStatus::Modified,
            is_binary: false,
            hunks: vec![wt_core::diff::DiffHunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: vec![wt_core::diff::DiffLine {
                    kind: DiffLineKind::Context,
                    content: "unchanged".to_string(),
                }],
            }],
            truncated: false,
        }
    }

    #[test]
    fn cache_identity_guard_rejects_a_mismatched_cache_entry() {
        let file_a = sample_diff_file("a.rs");
        let file_b = sample_diff_file("b.rs"); // Same shape as `file_a`, different real path.
        let per_hunk = vec![code_view::highlight_block(
            ["unchanged"],
            Some("rs"),
            code_view::HighlightOptions::default(),
        )];
        let per_hunk_numbers = vec![vec![(Some(1), Some(1))]];
        let cache = Some((file_a, per_hunk, per_hunk_numbers));

        assert!(
            diff_highlight_cache_for(&cache, &file_b).is_none(),
            "a cache built for a.rs must never be treated as usable for rendering b.rs, even \
             though they have byte-identical hunk/line shape - the real, checked identity guard \
             this function exists to provide"
        );
    }

    #[test]
    fn cache_identity_guard_accepts_a_matching_cache_entry() {
        let file = sample_diff_file("a.rs");
        let per_hunk = vec![code_view::highlight_block(
            ["unchanged"],
            Some("rs"),
            code_view::HighlightOptions::default(),
        )];
        let per_hunk_numbers = vec![vec![(Some(1), Some(1))]];
        let cache = Some((file.clone(), per_hunk, per_hunk_numbers));

        let (cached_per_hunk, cached_numbers) = diff_highlight_cache_for(&cache, &file)
            .expect("a cache built for exactly this file must be usable");
        assert_eq!(cached_per_hunk.len(), 1);
        assert_eq!(cached_numbers[0][0], (Some(1), Some(1)));
    }

    #[test]
    fn cache_identity_guard_handles_no_cache_yet() {
        let file = sample_diff_file("a.rs");
        let cache: Option<DiffHighlightCache> = None;
        assert!(diff_highlight_cache_for(&cache, &file).is_none());
    }
}

/// The pure half of GitHub issue #224: [`diff_rows`], the flat row plan
/// [`AdeApp::render_diff_file_detail`]'s `uniform_list` is both sized by and drawn from.
#[cfg(test)]
mod diff_row_plan_tests {
    use super::*;

    fn line(kind: DiffLineKind, content: &str) -> wt_core::diff::DiffLine {
        wt_core::diff::DiffLine {
            kind,
            content: content.to_string(),
        }
    }

    fn file_with(hunks: Vec<wt_core::diff::DiffHunk>, truncated: bool) -> DiffFile {
        DiffFile {
            path: PathBuf::from("src/main.rs"),
            old_path: None,
            status: wt_core::diff::FileChangeStatus::Modified,
            is_binary: false,
            hunks,
            truncated,
        }
    }

    #[test]
    fn the_row_plan_interleaves_headers_fold_markers_and_lines_in_scroll_order() {
        let file = file_with(
            vec![
                wt_core::diff::DiffHunk {
                    header: "@@ -1,2 +1,2 @@".to_string(),
                    lines: vec![
                        line(DiffLineKind::Removed, "old"),
                        line(DiffLineKind::Added, "new"),
                    ],
                },
                // New-side range starts at 40, while the first hunk covered 1..=2 - a real
                // 37-line unchanged gap (`changes::fold_gap_between`).
                wt_core::diff::DiffHunk {
                    header: "@@ -40,1 +40,1 @@".to_string(),
                    lines: vec![line(DiffLineKind::Context, "unchanged")],
                },
            ],
            false,
        );

        assert_eq!(
            diff_rows(&file, &[]),
            vec![
                DiffRow::HunkHeader(0),
                DiffRow::Line {
                    hunk: 0,
                    line: 0,
                    row: 0
                },
                DiffRow::Line {
                    hunk: 0,
                    line: 1,
                    row: 1
                },
                DiffRow::FoldMarker {
                    gap: 37,
                    before_hunk: 1
                },
                DiffRow::HunkHeader(1),
                DiffRow::Line {
                    hunk: 1,
                    line: 0,
                    row: 2
                },
            ],
            "the plan must be exactly what the pre-virtualization render loop appended, in the \
             same order - including the flat `row` counter the `diff-line-{{n}}` selectors and \
             the render cap are both expressed in, which keeps counting across hunk boundaries"
        );
    }

    #[test]
    fn back_to_back_hunks_produce_no_fold_marker() {
        let file = file_with(
            vec![
                wt_core::diff::DiffHunk {
                    header: "@@ -1,5 +1,5 @@".to_string(),
                    lines: vec![line(DiffLineKind::Context, "a")],
                },
                wt_core::diff::DiffHunk {
                    header: "@@ -6,5 +6,5 @@".to_string(),
                    lines: vec![line(DiffLineKind::Context, "b")],
                },
            ],
            false,
        );
        assert!(
            !diff_rows(&file, &[])
                .iter()
                .any(|row| matches!(row, DiffRow::FoldMarker { .. })),
            "there is no unchanged span between these two hunks, so there must be no `⋯ N \
             unchanged lines` row claiming one"
        );
    }

    #[test]
    fn the_row_plan_stops_at_the_render_cap_and_ends_with_the_truncation_notice() {
        let lines: Vec<wt_core::diff::DiffLine> = (0..MAX_RENDERED_DIFF_LINES_PER_FILE + 50)
            .map(|index| line(DiffLineKind::Added, &format!("line {index}")))
            .collect();
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -1,1 +1,350 @@".to_string(),
                lines,
            }],
            false,
        );

        let rows = diff_rows(&file, &[]);
        let line_rows = rows
            .iter()
            .filter(|row| matches!(row, DiffRow::Line { .. }))
            .count();
        assert_eq!(
            line_rows, MAX_RENDERED_DIFF_LINES_PER_FILE,
            "the render cap must still be exactly what it was - a virtualized list is not a \
             reason to raise it, and raising it would be its own separate decision"
        );
        assert_eq!(
            rows.last(),
            Some(&DiffRow::Truncated),
            "a diff cut short by the cap must end with the truncation notice as the list's own \
             final item"
        );
    }

    #[test]
    fn a_loader_truncated_file_under_the_cap_still_ends_with_the_notice() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: vec![line(DiffLineKind::Context, "unchanged")],
            }],
            true,
        );
        assert_eq!(diff_rows(&file, &[]).last(), Some(&DiffRow::Truncated));
    }

    #[test]
    fn a_note_row_interleaves_directly_beneath_the_line_it_is_anchored_to() {
        // `@@ -4,2 +4,2 @@`: old lines 4-5, new lines 4-5. The removed line is old 4, the added
        // line is new 4.
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -4,2 +4,2 @@".to_string(),
                lines: vec![
                    line(DiffLineKind::Removed, "old"),
                    line(DiffLineKind::Added, "new"),
                    line(DiffLineKind::Context, "same"),
                ],
            }],
            false,
        );

        assert_eq!(
            diff_rows(&file, &[NoteAnchor::New(4)]),
            vec![
                DiffRow::HunkHeader(0),
                DiffRow::Line {
                    hunk: 0,
                    line: 0,
                    row: 0
                },
                DiffRow::Line {
                    hunk: 0,
                    line: 1,
                    row: 1
                },
                DiffRow::Note {
                    anchor: NoteAnchor::New(4),
                    note_row: 0
                },
                DiffRow::Line {
                    hunk: 0,
                    line: 2,
                    row: 2
                },
            ],
            "the note sits between its own line and the next one, and the diff's own flat `row` \
             counter is untouched by it - a note is not a diff line and must not shift the \
             selectors or the cap"
        );
    }

    #[test]
    fn a_note_on_a_removed_line_pins_under_the_removed_line() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -4,2 +4,2 @@".to_string(),
                lines: vec![
                    line(DiffLineKind::Removed, "old"),
                    line(DiffLineKind::Added, "new"),
                ],
            }],
            false,
        );

        let rows = diff_rows(&file, &[NoteAnchor::Old(4)]);
        let note_at = rows
            .iter()
            .position(|row| matches!(row, DiffRow::Note { .. }))
            .expect("the note must be planned");
        assert_eq!(
            rows[note_at - 1],
            DiffRow::Line {
                hunk: 0,
                line: 0,
                row: 0
            },
            "an `Old` anchor pins under the removed line, not under the added line that happens \
             to carry the same number on the other side of the diff"
        );
    }

    #[test]
    fn an_anchor_no_longer_in_the_diff_plans_no_note_row() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -4,1 +4,1 @@".to_string(),
                lines: vec![line(DiffLineKind::Context, "same")],
            }],
            false,
        );
        assert!(
            !diff_rows(&file, &[NoteAnchor::New(900)])
                .iter()
                .any(|row| matches!(row, DiffRow::Note { .. })),
            "a note whose line is not on screen is simply not drawn - never drawn somewhere else"
        );
    }

    #[test]
    fn diff_order_interleaves_removed_and_added_anchors_the_way_the_hunk_does() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -4,3 +4,3 @@".to_string(),
                lines: vec![
                    line(DiffLineKind::Context, "context"),
                    line(DiffLineKind::Removed, "old"),
                    line(DiffLineKind::Added, "new"),
                ],
            }],
            false,
        );
        assert_eq!(
            note_anchors_in_diff_order(&file),
            vec![NoteAnchor::New(4), NoteAnchor::Old(5), NoteAnchor::New(5),],
            "sorting the anchors themselves would have put both `New`s before the `Old`, which is \
             not the order the lines are on screen"
        );
    }

    #[test]
    fn a_complete_diff_gets_no_truncation_notice() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: vec![line(DiffLineKind::Context, "unchanged")],
            }],
            false,
        );
        assert!(!diff_rows(&file, &[]).contains(&DiffRow::Truncated));
    }
}

/// Real, live-rendered proof that the Diff view's row list is genuinely virtualized (GitHub issue
/// #224, "Diff file view is lagging") - that a row scrolled far below the viewport is not merely
/// *invisible* but never becomes a painted element at all.
#[cfg(test)]
mod diff_virtualization_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};
    use test_support::git;

    /// A real repository whose working tree differs from its committed base by `added` brand-new
    /// lines in one file - a single real git hunk, `added` lines long.
    fn seed_big_diff(dir: &std::path::Path, added: usize) {
        test_support::seed_empty_repo_at(dir);
        std::fs::write(dir.join("big.rs"), "fn noop() {}\n").expect("write big.rs");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);
        git(dir, &["checkout", "-b", "feature"]);
        let mut content = String::from("fn noop() {}\n");
        for index in 0..added {
            content.push_str(&format!("fn generated_{index}() -> i32 {{ {index} }}\n"));
        }
        std::fs::write(dir.join("big.rs"), &content).expect("rewrite big.rs");
    }

    /// A real repository whose working tree changes one line near the top and one near the bottom
    /// of a 60-line file - two real git hunks with a real unchanged span between them, which is
    /// what makes `changes::fold_gap_between` produce a genuine `⋯ N unchanged lines` row.
    fn seed_two_hunk_diff(dir: &std::path::Path) {
        test_support::seed_empty_repo_at(dir);
        let original: String = (1..=60).map(|n| format!("fn f{n}() {{ {n} }}\n")).collect();
        std::fs::write(dir.join("two.rs"), &original).expect("write two.rs");
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);
        git(dir, &["checkout", "-b", "feature"]);
        let changed: String = (1..=60)
            .map(|n| {
                if n == 2 || n == 58 {
                    format!("fn f{n}() {{ {} }}\n", n * 1000)
                } else {
                    format!("fn f{n}() {{ {n} }}\n")
                }
            })
            .collect();
        std::fs::write(dir.join("two.rs"), &changed).expect("rewrite two.rs");
    }

    fn open_diff_on<'a>(
        cx: &'a mut TestAppContext,
        repo: &std::path::Path,
        path: &str,
    ) -> (Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = open_test_app(cx, repo.to_path_buf());
        cx.run_until_parked();
        let path = PathBuf::from(path);
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(path, window, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    /// A deliberately huge delta: `uniform_list` clamps to its own real maximum scroll offset, so
    /// this lands at the true bottom without modelling row heights or the viewport.
    fn scroll_to_bottom(cx: &mut gpui::VisualTestContext, anchor: gpui::Point<Pixels>) {
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: anchor,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_diff_line_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = temp_repo();
        seed_big_diff(repo.path(), 350);
        let (_app, cx) = open_diff_on(cx, repo.path(), "big.rs");

        assert!(
            cx.debug_bounds("diff-line-0").is_some(),
            "the first diff row must really paint - if it doesn't, this test proves nothing \
             about virtualization, only that the diff is empty"
        );
        assert!(
            cx.debug_bounds("diff-line-250").is_none(),
            "row 250 is far below any plausible viewport, so a virtualized list must never build \
             it as an element at all - this is exactly what the pre-fix eager `flex_col` did, and \
             what this assertion was checked to genuinely fail against"
        );
    }

    #[gpui::test]
    fn scrolling_the_virtualized_diff_materializes_a_row_that_was_not_painted(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        seed_big_diff(repo.path(), 350);
        let (_app, cx) = open_diff_on(cx, repo.path(), "big.rs");

        let first_row = cx
            .debug_bounds("diff-line-0")
            .expect("the first diff row must really paint");
        assert!(
            cx.debug_bounds("diff-line-299").is_none(),
            "precondition: a row far below the viewport must not be painted before scrolling"
        );

        scroll_to_bottom(cx, first_row.center());

        assert!(
            cx.debug_bounds("diff-line-299").is_some(),
            "scrolling to the bottom must really materialize a row that was absent - if this \
             fails the list is not scrollable any more, which is a far worse regression than the \
             per-frame render cost this change set out to fix"
        );
        assert!(
            cx.debug_bounds("diff-line-0").is_none(),
            "and the rows scrolled off the top must stop being built, not merely move - a list \
             that keeps painting them is not virtualizing, it is just translating"
        );
    }

    #[gpui::test]
    fn hunk_headers_and_fold_markers_paint_at_the_shared_row_height(cx: &mut TestAppContext) {
        let repo = temp_repo();
        seed_two_hunk_diff(repo.path());
        let (app, cx) = open_diff_on(cx, repo.path(), "two.rs");

        // Precondition, read off the real loaded diff rather than assumed: git really did produce
        // two hunks here, with a real unchanged gap between them.
        app.read_with(cx, |app, _| {
            let file = app
                .open_diff_file_cache
                .as_ref()
                .expect("the diff must be loaded");
            assert_eq!(
                file.hunks.len(),
                2,
                "precondition: this seed must produce exactly two real hunks, got {:?}",
                file.hunks.iter().map(|h| &h.header).collect::<Vec<_>>()
            );
            assert!(
                diff_rows(file, &[])
                    .iter()
                    .any(|row| matches!(row, DiffRow::FoldMarker { .. })),
                "precondition: the two hunks must have a real unchanged span between them"
            );
        });

        let header = cx
            .debug_bounds("diff-hunk-header-0")
            .expect("the first hunk's header must really paint inside the virtualized list");
        let first_line = cx
            .debug_bounds("diff-line-0")
            .expect("the first diff row must really paint");
        let fold = cx
            .debug_bounds("diff-fold-marker-1")
            .expect("the fold marker between the two hunks must really paint");
        let second_header = cx
            .debug_bounds("diff-hunk-header-1")
            .expect("the second hunk's header must really paint");

        assert_eq!(
            header.size.height, first_line.size.height,
            "the hunk header is item 0, the one row every other slot's height is measured from - \
             it must be exactly as tall as a real diff line"
        );
        assert_eq!(
            fold.size.height, first_line.size.height,
            "and so must the fold marker, which is a different element with its own `rems(0.85)` \
             text"
        );
        assert_eq!(
            first_line.origin.y,
            header.origin.y + header.size.height,
            "the first diff line must sit directly below its own hunk's header, with no gap - \
             the list lays its items out contiguously"
        );
        assert_eq!(
            second_header.origin.y,
            fold.origin.y + fold.size.height,
            "and the second hunk's header directly below the fold marker that introduces it"
        );
    }

    #[gpui::test]
    fn the_truncation_notice_is_the_last_item_of_the_list_at_the_shared_row_height(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        seed_big_diff(repo.path(), 350);
        let (_app, cx) = open_diff_on(cx, repo.path(), "big.rs");

        let first_row = cx
            .debug_bounds("diff-line-0")
            .expect("the first diff row must really paint");
        assert!(
            cx.debug_bounds("diff-truncated-row").is_none(),
            "precondition: the notice lives at the very bottom of a 300-row list, so it must not \
             be painted while the list is scrolled to the top"
        );

        scroll_to_bottom(cx, first_row.center());

        let last_row = cx
            .debug_bounds("diff-line-299")
            .expect("the last row within the render cap must paint at the bottom of the list");
        let notice = cx
            .debug_bounds("diff-truncated-row")
            .expect("a diff cut short by the render cap must say so at its bottom");
        assert_eq!(
            notice.origin.y,
            last_row.origin.y + last_row.size.height,
            "the notice must be the item directly below the last rendered diff line"
        );
        assert_eq!(
            notice.size.height, last_row.size.height,
            "and exactly as tall as every other item, which is the fixed height `uniform_list` \
             lays every slot out at"
        );
    }

    #[gpui::test]
    fn the_overlay_scrollbar_still_tracks_the_virtualized_list(cx: &mut TestAppContext) {
        let repo = temp_repo();
        seed_big_diff(repo.path(), 350);
        let (app, cx) = open_diff_on(cx, repo.path(), "big.rs");
        // The scrollbar is built from the geometry the *previous* frame's layout wrote onto the
        // handle, so a second real frame is what makes it appear - the same one-frame lag every
        // other overlay scrollbar in this app has.
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("diff-view-scrollbar").is_some(),
            "a 300-row diff overflows its pane, so the real overlay scrollbar must paint - if \
             this fails the handle's geometry is no longer reaching it"
        );

        // A diff that genuinely fits must still get no scrollbar at all.
        let small = temp_repo();
        test_support::seed_empty_repo_at(small.path());
        std::fs::write(small.path().join("small.rs"), "fn a() -> i32 {\n    1\n}\n")
            .expect("write small.rs");
        git(small.path(), &["add", "."]);
        git(small.path(), &["commit", "-m", "initial"]);
        git(small.path(), &["checkout", "-b", "feature"]);
        std::fs::write(small.path().join("small.rs"), "fn a() -> i32 {\n    2\n}\n")
            .expect("rewrite small.rs");
        let (app, cx) = open_diff_on(cx, small.path(), "small.rs");
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("diff-line-0").is_some(),
            "precondition: the small diff really is open and rendering rows"
        );
        assert!(
            cx.debug_bounds("diff-view-scrollbar").is_none(),
            "a four-row diff doesn't overflow anything, so there must be no scrollbar for it"
        );
    }

    #[gpui::test]
    fn every_virtualized_row_resolves_its_own_line_through_the_identity_guard(
        cx: &mut TestAppContext,
    ) {
        let repo = temp_repo();
        seed_two_hunk_diff(repo.path());
        let (app, cx) = open_diff_on(cx, repo.path(), "two.rs");

        app.read_with(cx, |app, _| {
            let file = app
                .open_diff_file_cache
                .as_ref()
                .expect("the diff must be loaded");
            let (per_hunk, per_hunk_numbers) =
                diff_highlight_cache_for(&app.diff_highlight_cache, file)
                    .expect("the guard must accept the cache built for this very file");

            let mut checked_second_hunk = 0usize;
            for row in diff_rows(file, &[]) {
                let DiffRow::Line { hunk, line, .. } = row else {
                    continue;
                };
                let expected = &file.hunks[hunk].lines[line].content;
                let rendered = per_hunk
                    .get(hunk)
                    .and_then(|lines| lines.get(line))
                    .unwrap_or_else(|| {
                        panic!("row (hunk {hunk}, line {line}) must resolve real highlighting")
                    });
                assert_eq!(
                    &rendered.text, expected,
                    "the highlighted text a virtualized row resolves must be that row's own \
                     line - anything else is the class of bug the identity guard exists to \
                     prevent, reintroduced through per-row indexing"
                );
                assert!(
                    per_hunk_numbers
                        .get(hunk)
                        .and_then(|nums| nums.get(line))
                        .is_some(),
                    "and its gutter numbers must be resolvable at the same (hunk {hunk}, line \
                     {line}) position, from the index-aligned second half of the same cache"
                );
                if hunk == 1 {
                    checked_second_hunk += 1;
                }
            }
            assert!(
                checked_second_hunk > 0,
                "this test is only meaningful if it really checked rows of the *second* hunk - \
                 those are the ones a per-row indexing mistake would misroute"
            );
        });
    }
}
