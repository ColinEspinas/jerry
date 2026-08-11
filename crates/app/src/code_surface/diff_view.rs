//! The read-only Diff view: one file's hunks as real syntax-highlighted rows, its gutter,
//! its fold markers, and the highlight cache that keeps re-rendering it cheap.

use super::zoom::zoom_scoped;
use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_sidebar_message;
use std::rc::Rc;

/// Every row of the Diff view's virtualized list, in the flat order it scrolls in - built once
/// per frame by [`diff_rows`] and then indexed by `uniform_list`'s row builder.
///
/// The same shape (and the same reason) as `crate::sidebar::render`'s own `TreeRow`: the row
/// builder closure is `'static` and cannot hold a borrow of `self` or of the `&DiffFile` being
/// rendered, so a row names *where* its content lives rather than carrying the content itself.
/// It is `Copy` for the same reason - nothing here owns a `String`.
///
/// The three real row kinds this list interleaves (hunk header, fold marker, diff line) all
/// render at the same `rems(1.6)` height, which is `uniform_list`'s one real requirement - see
/// [`AdeApp::render_diff_file_detail`]'s own docs on how that is enforced.
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
    /// The trailing `... diff truncated for this file` notice - a genuine final *item* of the
    /// list, not a sibling below it (see [`render_diff_truncated_row`]).
    Truncated,
}

/// The Diff view's whole row plan for `file`, in scroll order: fold markers between hunks that
/// have a real unchanged gap, each hunk's header, that hunk's lines up to
/// [`MAX_RENDERED_DIFF_LINES_PER_FILE`], and a trailing truncation notice when this file's diff
/// really was cut short (either by that cap here or by `wt_core::diff`'s own load-time cap,
/// `DiffFile::truncated`).
///
/// Pure, and separate from [`AdeApp::render_diff_file_detail`], for two reasons: `uniform_list`
/// needs a real item *count* before it ever calls its row builder, and both numbers have to come
/// from the same plan or the list would be sized by one shape and drawn from another. Being pure
/// also makes the interleaving directly `#[test]`-able without a GPUI window - see
/// `diff_row_plan_tests`.
///
/// The cap is applied exactly where the pre-virtualization render loop applied it, including its
/// one subtlety: the cap is only checked *inside* a hunk's line loop, so a hunk whose first line
/// would exceed it still contributes its header (and fold marker) before the plan stops. That is
/// deliberately preserved rather than "cleaned up" - it is what makes the truncation notice read
/// as sitting under a real hunk boundary instead of mid-hunk.
fn diff_rows(file: &DiffFile) -> Vec<DiffRow> {
    // At most one header + one fold marker per hunk, the capped line count, and the notice.
    let mut rows: Vec<DiffRow> =
        Vec::with_capacity(file.hunks.len() * 2 + MAX_RENDERED_DIFF_LINES_PER_FILE + 1);
    let mut rendered_lines = 0usize;
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
        }
    }

    if file.truncated || hunks_truncated {
        rows.push(DiffRow::Truncated);
    }
    rows
}

/// Which surface [`AdeApp::render_diff_file_detail`] is drawing into.
///
/// GitHub issue #225 introduced a second place that shows a file's hunks - the agent Review tab -
/// and the decision was explicitly **not** to write a second diff renderer for it. There is one
/// hunk renderer in this app, and this parameter is the whole of what differs between its two
/// callers: which highlight cache to read, which scroll handle to drive, and which element-id/
/// `debug_selector` prefix to use so the two surfaces' elements stay distinguishable.
///
/// The three resources are genuinely per-surface, not shared, because both surfaces can hold a
/// *different* open file at the same time: one shared highlight cache would have its identity
/// guard reject every read as the user moved between them (recomputing both files' highlighting
/// on every switch), and one shared scroll handle would drag each surface's scroll position onto
/// the other.
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
    ///
    /// The row builder inside [`AdeApp::render_diff_file_detail`]'s `uniform_list` is `'static`
    /// (GitHub issue #224) and so cannot hold a borrow of the `&DiffFile` the method itself was
    /// handed - it re-resolves through this instead, the same way the pre-#225 Changes-only
    /// closure re-resolved `self.open_diff_file_cache` directly. Reading through `surface` rather
    /// than hardcoding that field is what lets one virtualized list implementation serve both
    /// surfaces correctly.
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
    ///
    /// Highlights at most [`MAX_RENDERED_DIFF_LINES_PER_FILE`] lines total, hunk by hunk,
    /// truncating the last hunk's own fed-in line list once the cap is reached -
    /// [`Self::render_diff_file_detail`]'s render loop never shows more than that many lines
    /// either, so highlighting a file's full, uncapped (up to `wt_core::diff`'s own
    /// `MAX_HUNK_LINES_PER_FILE` per hunk) hunk list would do real work no render could ever
    /// show. Measured directly against what was then this crate's own largest real `.rs`
    /// file (the ~3,900-line `root/code_surface.rs`, since split into this folder) in a
    /// debug build: highlighting it whole took ~80ms; capped to this
    /// constant (300 lines, split across several hunk-sized calls) took ~5-6ms - the real,
    /// measured reason this stays a synchronous call at a real, infrequent change point rather
    /// than needing a background task.
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
    ///
    /// ## Cache identity guard
    ///
    /// `self.diff_highlight_cache` is read through a real `file`-identity filter (`cache`,
    /// below) before anything positional (`per_hunk.get(hunk_index)`/`lines.get(line_index)`) is
    /// read from it - never read positionally on its own. Without this, a cache that's ever even
    /// briefly stale relative to `file` (e.g. a future caller racing a fast switch between two
    /// open diffs) wouldn't just show wrong *colors* - `hunk_index`/`line_index` would be valid
    /// positions into a *different* file's real source lines and gutter numbers, rendered under
    /// the *current* file's correct diff signs, the single most misleading output this surface
    /// could show. When the filter fails (mismatched or not-yet-built), `cache` is `None` for
    /// this whole render pass, so every line falls back to [`render_diff_line`]'s own plain-text/
    /// blank-gutter path - real, honestly-blank output, never another file's real content. The
    /// guard itself is [`diff_highlight_cache_for`], factored out as its own pure, directly
    /// unit-tested function - see its own docs and tests for the constructed-mismatch proof.
    ///
    /// The guard is consulted *inside* the row builder, once per invocation of it, rather than
    /// once per frame in this method: the builder is `'static` and re-resolves the open
    /// `DiffFile` through `surface.open_file(self)` (below), so the cache has to be filtered
    /// against that same re-resolved file for the check to mean anything. Nothing about the check
    /// itself changed - same function, same `None`-means-fall-back-to-plain-text contract, and
    /// the per-row `per_hunk.get(hunk)`/`lines.get(line)` reads are still reachable only through
    /// it.
    ///
    /// ## Virtualization (GitHub issue #224), generalized to two surfaces (GitHub issue #225)
    ///
    /// A real `gpui::uniform_list`, following
    /// `crate::code_surface::file_view::AdeApp::render_file_view`'s established pattern for this
    /// same content shape (see `vendor/zed/crates/gpui/examples/uniform_list.rs` for the API
    /// itself). Until issue #224 every row of the whole capped diff - up to
    /// [`MAX_RENDERED_DIFF_LINES_PER_FILE`] lines plus every hunk header and fold marker - was
    /// built, laid out and painted on *every single frame*, inside a plain
    /// `div().overflow_y_scroll()`, including the great majority scrolled off screen. That is
    /// structurally the same per-frame cost `crate::sidebar::render::AdeApp::render_file_tree`
    /// and `crate::graph_view::render::AdeApp::render_graph_rows` (GitHub issue #218) were
    /// carrying before they were virtualized; the file tree's own measurement of it (~72% of a
    /// whole `Window::draw`) is *the file tree's*, on its row shape, not a measurement of this
    /// view. What is measured here is `diff_virtualization_tests`' own before/after: a row far
    /// below the viewport stopped being built at all.
    ///
    /// `surface` is what makes one `uniform_list` implementation correct for both the git Diff
    /// view and the agent Review tab (issue #225): every place this method would otherwise reach
    /// for `self.open_diff_file_cache`/`self.diff_highlight_cache`/`self.diff_view_scroll_handle`
    /// directly instead goes through `surface.open_file`/`surface.highlight_cache`/
    /// `surface.scroll_handle`, so the two surfaces can each hold a different open file without
    /// either one's render pass reading the other's state.
    ///
    /// Two things `uniform_list` requires, both real:
    /// - **Definite height.** Its default `ListSizingBehavior::Auto` gives it zero intrinsic
    ///   height, so every pixel comes from the `.flex_1().min_h_0()` below - drop either and it
    ///   renders zero rows, silently. It also owns its own scroll offset (hence both
    ///   [`Self::diff_view_scroll_handle`] and [`Self::review_scroll_handle`] being a
    ///   `gpui::UniformListScrollHandle`), so it needs no `overflow_y_scroll()` wrapper and must
    ///   not be given one.
    /// - **One uniform row height,** measured from item 0 alone and then used for every slot
    ///   (`vendor/zed/crates/gpui/src/elements/uniform_list.rs`'s `measure_item`/`prepaint`).
    ///   This list interleaves four differently-purposed rows, so each is pinned to the same
    ///   `rems(1.6)` a diff line's own `line_height` already gives it: [`render_fold_marker`]
    ///   already had `.h(rems(1.6))`, [`render_hunk_header`] and [`render_diff_truncated_row`]
    ///   were given it here. `rems`, not `px`, so they all still scale together under
    ///   [`zoom_scoped`] - which wraps the list exactly the way `render_file_view` wraps its own.
    ///   A row that disagreed would simply be clipped, with no panic and no warning.
    pub(crate) fn render_diff_file_detail(
        &self,
        file: &DiffFile,
        surface: DiffDetailSurface,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Read the effective zoom once and pass it to `zoom_scoped` at every return point below.
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
        let rows: Rc<Vec<DiffRow>> = Rc::new(diff_rows(file));
        let row_count = rows.len();

        let list = uniform_list(
            // Per-surface and per-path (see `DiffDetailSurface::id_prefix`'s own docs): a
            // different open diff, or the other surface entirely, is a different list, not the
            // same one showing new content.
            format!("{}-detail-{}", surface.id_prefix(), file.path.display()),
            row_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, _cx| {
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
                                    render_diff_line(
                                        diff_line,
                                        rendered,
                                        numbers,
                                        surface.id_prefix(),
                                        row,
                                    )
                                    .into_any_element()
                                }
                                None => render_blank_diff_row().into_any_element(),
                            }
                        }
                        DiffRow::Truncated => render_diff_truncated_row().into_any_element(),
                    })
                    .collect::<Vec<_>>()
            }),
        )
        // Load-bearing, and it fails silently if removed - see this method's own docs.
        .flex_1()
        .min_h_0()
        .bg(theme::surface::PTY)
        // GitHub issue #30's real overlay scrollbar reads its geometry straight off this same
        // handle (`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`).
        .track_scroll(surface.scroll_handle(self));

        zoom_scoped(rem_px, self.wrap_with_scrollbar(surface, list, cx))
    }

    /// Wraps the Diff view's `uniform_list` in the real, non-scrolling `.relative()` sibling
    /// wrapper GitHub issue #30's overlay scrollbar needs - see `crate::sidebar::render::AdeApp::
    /// render_file_tree`'s own docs on why the scrollbar must never be a child of the scrolling
    /// element itself.
    ///
    /// Only the real hunk-rendering path uses this now. The two message return points (binary,
    /// no-hunks) render through [`render_diff_message_pane`] instead, with no scrollbar at all:
    /// a one-line message can never overflow the pane, and now that this handle's geometry is
    /// written by the `uniform_list` alone, a message frame that still consulted it would be
    /// reading whatever the last *diff* left there - a scrollbar for content that isn't on
    /// screen.
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
            .children(self.render_vertical_scrollbar(
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
///
/// `.h(rems(1.6))` (with `.flex().items_center()`) rather than relying on its own line height:
/// this is item 0 of [`AdeApp::render_diff_file_detail`]'s `uniform_list` for every real diff -
/// the single row every other slot's height is measured from - so its height is stated outright
/// rather than left to emerge from the text style, and it is the same `rems(1.6)` a diff line's
/// `line_height` gives it. `rems`, not `px`, so it scales with zoom like the rows around it.
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
///
/// The final *item* of the row list rather than a sibling below it, which is what keeps it
/// scrolling with the rows it is talking about - the same treatment
/// `crate::graph_view::render::render_graph_load_more_row` gives the graph's own trailing notice
/// (GitHub issue #218/#221), and for the same reason it carries the shared `rems(1.6)` row
/// height: `uniform_list` sizes every slot from item 0 alone, so a taller row (which is exactly
/// what the `render_sidebar_message` this replaces was, at `p(px(10.0))` around 10.5px text)
/// would simply be clipped, with no panic and no warning. Same wording as before, in the same
/// faint monospace as the fold markers it sits below.
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
///
/// Sized in `rems()`, not `px()`: unlike the line-number gutter and git-gutter column, this
/// marker isn't exempt from zoom, so it must scale with the surrounding diff rows rather than
/// staying a fixed-size sliver once zoom moves off 100%. `0.85` keeps it proportionally smaller
/// than a diff line's own text, matching the 11px-vs-13px ratio at the 100% baseline.
///
/// Its `.h(rems(1.6))` - which predates virtualization - is also exactly the shared row height
/// [`AdeApp::render_diff_file_detail`]'s `uniform_list` lays every slot out at, so this marker
/// needed no height change to become a real item of that list. `before_hunk` names only this
/// row's own `diff-fold-marker-{before_hunk}` debug selector (see [`DiffRow::FoldMarker`]).
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
            "\u{22ef} {gap} unchanged line{}",
            if gap == 1 { "" } else { "s" }
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
///
/// `rendered`/`numbers` are `None`/`(None, None)` whenever [`render_diff_file_detail`]'s cache
/// identity guard couldn't confirm the cache actually belongs to the file being rendered (see
/// that method's own docs) - not just "shouldn't happen in practice", a real, checked condition.
/// This function stays honest either way: it falls back to `line`'s own raw, plainly-colored
/// content and a blank gutter rather than panicking, guessing, or - the failure mode this guard
/// exists to prevent - ever being handed (and blindly rendering) another file's real lines.
pub(in crate::code_surface) fn render_diff_line(
    line: &wt_core::diff::DiffLine,
    rendered: Option<&code_view::RenderedLine>,
    numbers: (Option<usize>, Option<usize>),
    selector_prefix: &'static str,
    row_index: usize,
) -> impl IntoElement {
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

    let mut row = div()
        .flex()
        .items_center()
        .font(font(theme::font::MONO))
        .text_size(rems(1.0))
        .line_height(rems(1.6))
        // `debug_selector` is a no-op outside test builds; lets a real render test measure this
        // row's painted bounds and confirm the diff view's own rows are genuinely reachable, the
        // same pattern `render_file_view_line`'s `file-view-text-row-{n}` selector already
        // establishes for the File view.
        .debug_selector(move || format!("{selector_prefix}-line-{row_index}"));
    if let Some(bg) = bg {
        row = row.bg(bg);
    }
    row = row.child(
        div()
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

    row.child(render_diff_gutter_number(numbers.0))
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
}

/// Real, render-level coverage for the Diff view's per-token syntax highlighting and its
/// caching (`AdeApp::diff_highlight_cache`/`ensure_diff_highlight_cache`) - `render_diff_line`'s
/// entire output shape changed in Revision R9a and, until this module existed, not one test
/// actually rendered a real diff and checked anything about it.
#[cfg(test)]
mod diff_render_tests {
    use super::*;
    use gpui::TestAppContext;

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

    /// Renders a real diff of a real `.rs` file (one line changed - a real, git-produced
    /// context/removed/added hunk) and checks real things about the result: every row really
    /// painted (`debug_selector`), and the cache the render path reads from
    /// (`AdeApp::diff_highlight_cache`) really contains per-token classification - a `fn`
    /// keyword and the changed integer literal - not flat, uncoloured text.
    #[gpui::test]
    fn opening_a_real_diff_renders_real_syntax_highlighted_rows(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
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

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// Proves `AdeApp::diff_highlight_cache` is genuinely *reused*, not silently recomputed
    /// every time `Self::ensure_diff_highlight_cache` runs - pointer identity of the cached
    /// `Vec`, since a fresh recompute would allocate a new one (mirrors
    /// `code_view_cache_tests::repeated_renders_of_the_same_open_file_reuse_the_cached_parse`'s
    /// identical technique for `file_view_cache`). If the `DiffFile` freshness check were ever
    /// removed from `ensure_diff_highlight_cache`, this would fail.
    #[gpui::test]
    fn repeated_refreshes_of_the_same_open_diff_reuse_the_cached_highlighting(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
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

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

        // The real hook this cache is recomputed from, called again with nothing changed.
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

    /// The other half of the same cache's correctness: switching to a *different* changed file
    /// must genuinely recompute - not a cache that never refreshes.
    #[gpui::test]
    fn switching_the_open_diff_to_a_different_file_recomputes_the_highlight_cache(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.rs"), "fn a() -> i32 {\n    1\n}\n")
            .expect("write a.rs");
        std::fs::write(repo.path().join("b.py"), "def b():\n    return 1\n").expect("write b.py");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.rs"), "fn a() -> i32 {\n    2\n}\n")
            .expect("rewrite a.rs");
        std::fs::write(repo.path().join("b.py"), "def b():\n    return 2\n").expect("rewrite b.py");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// Regression for the highlight cache's real `MAX_RENDERED_DIFF_LINES_PER_FILE` cap: a diff
    /// with more lines than the render loop will ever show must still render every one of the
    /// lines it *does* show with real highlighting (not a `None`-cache fallback row), and must
    /// not panic at the exact truncation boundary.
    ///
    /// GitHub issue #224 changed *when* the last row within the cap paints, not whether it is
    /// reachable: the list is a real `gpui::uniform_list` now, so row 299 is only built once it
    /// is genuinely scrolled into view. The assertion below therefore scrolls first - and the
    /// row past the cap must still not exist at any scroll position at all, which is the half of
    /// this test that is really about the cap.
    #[gpui::test]
    fn a_diff_past_the_rendered_line_cap_still_highlights_every_line_it_actually_renders(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("big.rs"), "fn noop() {}\n").expect("write big.rs");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        // One hunk, 350 added lines - more than MAX_RENDERED_DIFF_LINES_PER_FILE (300).
        let mut content = String::from("fn noop() {}\n");
        for index in 0..350 {
            content.push_str(&format!("fn generated_{index}() -> i32 {{ {index} }}\n"));
        }
        std::fs::write(repo.path().join("big.rs"), &content).expect("rewrite big.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
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

    /// The CRITICAL fix's core proof: a cache built for `file_a` must never be read
    /// positionally for a render of `file_b`, even though both have the exact same hunk/line
    /// shape (so a purely positional `per_hunk.get(0).get(0)` lookup - the real bug this guard
    /// replaces - would "succeed" and silently hand back `file_a`'s real highlighted source
    /// text). `diff_highlight_cache_for` must reject the mismatch and return `None`, the signal
    /// [`AdeApp::render_diff_file_detail`] treats as "fall back to `file_b`'s own real, plain
    /// text" rather than ever painting `file_a`'s real content under `file_b`'s diff row.
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

    /// The other half: a cache that genuinely does belong to the file being rendered must still
    /// be usable - the guard must not reject real, fresh, matching cache entries too.
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

    /// No cache built yet (`None`) must also fall back cleanly, not panic - the same honest
    /// "nothing to read yet" case as a genuine mismatch.
    #[test]
    fn cache_identity_guard_handles_no_cache_yet() {
        let file = sample_diff_file("a.rs");
        let cache: Option<DiffHighlightCache> = None;
        assert!(diff_highlight_cache_for(&cache, &file).is_none());
    }
}

/// The pure half of GitHub issue #224: [`diff_rows`], the flat row plan
/// [`AdeApp::render_diff_file_detail`]'s `uniform_list` is both sized by and drawn from.
///
/// These need no GPUI window at all, which is the point of factoring the plan out of the render
/// method: the interleaving of hunk headers, fold markers, diff lines and the truncation notice -
/// and the exact place [`MAX_RENDERED_DIFF_LINES_PER_FILE`] cuts it off - is directly assertable
/// here, while `diff_virtualization_tests` below covers what only a real render can show.
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

    /// The ordinary shape: header, that hunk's lines, then - only where the headers say there is
    /// a real unchanged gap - a fold marker before the next header.
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
            diff_rows(&file),
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

    /// Back-to-back hunks (no real unchanged span between them) get no fold marker, so the plan
    /// can't invent scroll extent that isn't there.
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
            !diff_rows(&file)
                .iter()
                .any(|row| matches!(row, DiffRow::FoldMarker { .. })),
            "there is no unchanged span between these two hunks, so there must be no `⋯ N \
             unchanged lines` row claiming one"
        );
    }

    /// The cap is unchanged by virtualization (deliberately - GitHub issue #224 is about
    /// per-frame render cost, not about how much of a diff is reachable): exactly
    /// [`MAX_RENDERED_DIFF_LINES_PER_FILE`] line rows, then the truncation notice as the final
    /// item of the list.
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

        let rows = diff_rows(&file);
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

    /// The loader's own cap (`wt_core::diff`'s `DiffFile::truncated`) is a second, independent
    /// reason for that notice - a file well under this view's own render cap still gets it.
    #[test]
    fn a_loader_truncated_file_under_the_cap_still_ends_with_the_notice() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: vec![line(DiffLineKind::Context, "unchanged")],
            }],
            true,
        );
        assert_eq!(diff_rows(&file).last(), Some(&DiffRow::Truncated));
    }

    /// And a whole, untruncated diff must not grow a notice out of nowhere.
    #[test]
    fn a_complete_diff_gets_no_truncation_notice() {
        let file = file_with(
            vec![wt_core::diff::DiffHunk {
                header: "@@ -1,1 +1,1 @@".to_string(),
                lines: vec![line(DiffLineKind::Context, "unchanged")],
            }],
            false,
        );
        assert!(!diff_rows(&file).contains(&DiffRow::Truncated));
    }
}

/// Real, live-rendered proof that the Diff view's row list is genuinely virtualized (GitHub issue
/// #224, "Diff file view is lagging") - that a row scrolled far below the viewport is not merely
/// *invisible* but never becomes a painted element at all.
///
/// None of this is observable from [`diff_rows`]' pure plan, which is identical either way: only
/// a real render can tell "built 300 rows and clipped 250 of them" apart from "built 50". These
/// tests therefore also assert the positive half - that the rows which should paint really do,
/// that the absent ones are still reachable by really scrolling, and that the list's three other
/// row kinds (hunk header, fold marker, truncation notice) survived being folded into a list that
/// lays every slot out at one fixed height - so a future change that "virtualizes" by rendering
/// nothing, or by silently clipping every non-line row, fails here rather than passing.
///
/// The first two were run against the pre-fix eager `flex_col` before being committed, and both
/// genuinely failed against it: with 350 real changed lines open, `diff-line-250` had painted
/// bounds with the list untouched at the top, and so had `diff-line-299`, the very last row
/// within the render cap. Both pass against the `uniform_list`. That is what they measure and all
/// they measure - no frame timing is claimed here, for this view.
///
/// Mirrors `crate::graph_view::render::graph_virtualization_tests`, the same proof for the git
/// graph's commit rows.
#[cfg(test)]
mod diff_virtualization_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &std::path::Path) {
        git(dir, &["init", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test User"]);
    }

    /// A real repository whose working tree differs from its committed base by `added` brand-new
    /// lines in one file - a single real git hunk, `added` lines long.
    ///
    /// 350 is deliberately more than [`MAX_RENDERED_DIFF_LINES_PER_FILE`] (300), so the same seed
    /// exercises the cap and the truncation notice; the test viewport is 1920x1080, so at the
    /// `rems(1.6)` row height (about 21px at the default editor font size) only on the order of
    /// 50 rows can be on screen at once - far fewer than either number.
    fn seed_big_diff(dir: &std::path::Path, added: usize) {
        init_repo(dir);
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
        init_repo(dir);
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
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.to_path_buf());
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
        let repo = tempfile::tempdir().expect("tempdir");
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

    /// The other half of "is it really virtualized": a row that legitimately isn't painted yet
    /// must still be reachable. This scrolls the real list with a real `gpui::ScrollWheelEvent`
    /// and asserts the row that was absent genuinely materializes - which simultaneously proves
    /// the list still scrolls at all now that the former `div().overflow_y_scroll()` wrapper is
    /// gone, the one behaviour this change could plausibly have broken outright.
    #[gpui::test]
    fn scrolling_the_virtualized_diff_materializes_a_row_that_was_not_painted(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
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

    /// The three rows that are not diff lines have to survive being items of a list that measures
    /// item 0 and lays every other slot out at exactly that height: a hunk header (which *is*
    /// item 0 of every real diff), the `⋯ N unchanged lines` fold marker between two hunks, and
    /// the second hunk's own header after it. All three must paint, in that order, each exactly
    /// as tall as a real diff line - a row that disagreed would be clipped, with no panic and no
    /// warning.
    #[gpui::test]
    fn hunk_headers_and_fold_markers_paint_at_the_shared_row_height(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
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
                diff_rows(file)
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

    /// The truncation notice is the final *item* of the list rather than a sibling below it, so
    /// it has to sit directly under the last row within the cap and carry the same fixed height.
    /// (As a `render_sidebar_message` - what it used to be - it would have been a ~31px element
    /// in a ~21px slot, i.e. clipped.)
    #[gpui::test]
    fn the_truncation_notice_is_the_last_item_of_the_list_at_the_shared_row_height(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
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

    /// GitHub issue #30's overlay scrollbar has to keep working across the handle's type change
    /// (`gpui::ScrollHandle` -> `gpui::UniformListScrollHandle`): it reads its geometry through
    /// `crate::root::scrollbar::ScrollableHandle`, which already covered both kinds, but the
    /// values behind it are written by the `uniform_list` now rather than by a scrolling `div`.
    /// A diff long enough to overflow must still get a real, painted track - and one that fits
    /// must still get none, so this can't pass by drawing a scrollbar unconditionally.
    #[gpui::test]
    fn the_overlay_scrollbar_still_tracks_the_virtualized_list(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
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
        let small = tempfile::tempdir().expect("tempdir");
        init_repo(small.path());
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

    /// The cache identity guard under virtualization. The guard itself is unchanged
    /// ([`diff_highlight_cache_for`], still covered directly by `diff_render_tests`' own
    /// constructed-mismatch proofs) but *where* it is consulted moved: into the row builder,
    /// which now resolves one row at a time by `(hunk, line)` instead of walking hunks in order.
    /// That is exactly the indexing a virtualized rewrite could plausibly get wrong - and getting
    /// it wrong would reproduce the original CRITICAL bug's symptom, one line's real source text
    /// under another line's diff sign and gutter number.
    ///
    /// So this walks the real row plan for a real two-hunk diff and resolves each line row
    /// through the guard exactly the way the row builder does, asserting the text it lands on is
    /// that row's own content - including for the second hunk, whose rows are the ones a
    /// flat-counter mix-up would misroute.
    #[gpui::test]
    fn every_virtualized_row_resolves_its_own_line_through_the_identity_guard(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
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
            for row in diff_rows(file) {
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
