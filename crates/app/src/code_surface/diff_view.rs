//! The read-only Diff view: one file's hunks as real syntax-highlighted rows, its gutter,
//! its fold markers, and the highlight cache that keeps re-rendering it cheap.

use super::zoom::zoom_scoped;
use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_action_keycap_row;
use crate::root::widgets::render_sidebar_message;

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
    pub(in crate::code_surface) fn render_diff_file_detail(
        &self,
        file: &DiffFile,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Read the effective zoom once and pass it to `zoom_scoped` at every return point below.
        let rem_px = self.effective_code_rem_px();
        let mut container = div()
            .id(format!("diff-detail-{}", file.path.display()))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .bg(theme::surface::PTY)
            .py(px(4.0))
            // GitHub issue #30's real overlay scrollbar reads its geometry straight off this
            // same handle (`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`).
            .track_scroll(&self.diff_view_scroll_handle);

        if file.is_binary {
            return zoom_scoped(
                rem_px,
                self.wrap_with_scrollbar(
                    container.child(render_sidebar_message(
                        "binary file (contents not diffed)".to_string(),
                        theme::text::FAINT.into(),
                    )),
                    cx,
                ),
            );
        }

        // A rename-only file produces zero `@@` hunks, so falling through the loop below would
        // leave `container` with no children - a blank pane that looks like a rendering bug
        // rather than "nothing to show". `changes::empty_hunks_message` picks honest wording,
        // naming the rename specifically when that's the cause.
        if file.hunks.is_empty() {
            return zoom_scoped(
                rem_px,
                self.wrap_with_scrollbar(
                    container.child(render_sidebar_message(
                        changes::empty_hunks_message(file.status).to_string(),
                        theme::text::FAINT.into(),
                    )),
                    cx,
                ),
            );
        }

        // The real identity guard: a cache entry only counts as usable for this render pass if
        // it was built from this exact `file` (see this method's own docs, "Cache identity
        // guard", and `diff_highlight_cache_for`'s own docs/tests for the pure logic below).
        let cache = diff_highlight_cache_for(&self.diff_highlight_cache, file);

        let mut rendered_lines = 0usize;
        let mut hunks_truncated = false;
        let mut previous_header: Option<&str> = None;
        'hunks: for (hunk_index, hunk) in file.hunks.iter().enumerate() {
            if let Some(previous) = previous_header {
                if let Some(gap) = changes::fold_gap_between(previous, &hunk.header) {
                    container = container.child(render_fold_marker(gap));
                }
            }
            previous_header = Some(hunk.header.as_str());

            container = container.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(rems(1.0))
                    .line_height(rems(1.6))
                    .px(px(8.0))
                    .bg(theme::diff::HUNK_BG)
                    .text_color(theme::diff::HUNK_FG)
                    .child(hunk.header.clone()),
            );

            for (line_index, line) in hunk.lines.iter().enumerate() {
                if rendered_lines >= MAX_RENDERED_DIFF_LINES_PER_FILE {
                    hunks_truncated = true;
                    break 'hunks;
                }
                let row_index = rendered_lines;
                rendered_lines += 1;
                let rendered = cache
                    .and_then(|(per_hunk, _)| per_hunk.get(hunk_index))
                    .and_then(|lines| lines.get(line_index));
                let numbers = cache
                    .and_then(|(_, per_hunk_numbers)| per_hunk_numbers.get(hunk_index))
                    .and_then(|nums| nums.get(line_index))
                    .copied()
                    .unwrap_or((None, None));
                container = container.child(render_diff_line(line, rendered, numbers, row_index));
            }
        }

        if file.truncated || hunks_truncated {
            container = container.child(render_sidebar_message(
                "... diff truncated for this file".to_string(),
                theme::text::FAINT.into(),
            ));
        }

        zoom_scoped(rem_px, self.wrap_with_scrollbar(container, cx))
    }

    /// Wraps `content` (the Diff view's own scrollable `container`, already `track_scroll`'d with
    /// [`Self::diff_view_scroll_handle`]) in the real, non-scrolling `.relative()` sibling wrapper
    /// GitHub issue #30's overlay scrollbar needs - see `crate::sidebar::render::AdeApp::
    /// render_file_tree`'s own docs on why the scrollbar must never be a child of the scrolling
    /// element itself. Factored out because [`Self::render_diff_file_detail`] has three real
    /// return points (binary, no-hunks, and the real hunk-rendering path), and each needs the
    /// exact same wrap.
    fn wrap_with_scrollbar(
        &self,
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
                "diff-view-scrollbar",
                &self.diff_view_scroll_handle,
                &[],
                cx,
            ))
            .into_any_element()
    }
}

/// The diff view's `⋯ N unchanged lines` fold marker. `N` is derived from the hunks' `@@ ... @@`
/// headers (`crate::sidebar::changes::fold_gap_between`), never an estimate.
///
/// Sized in `rems()`, not `px()`: unlike the line-number gutter and git-gutter column, this
/// marker isn't exempt from zoom, so it must scale with the surrounding diff rows rather than
/// staying a fixed-size sliver once zoom moves off 100%. `0.85` keeps it proportionally smaller
/// than a diff line's own text, matching the 11px-vs-13px ratio at the 100% baseline.
pub(in crate::code_surface) fn render_fold_marker(gap: usize) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .h(rems(1.6))
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
        // wrapping and growing this row's height past its neighbours' (a real `uniform_list`-
        // adjacent risk elsewhere in this crate, even though this Diff view isn't itself
        // virtualized).
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
        .debug_selector(move || format!("diff-line-{row_index}"));
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

/// The File view toolbar's always-rendered `Accept file` button, always in its dimmed
/// non-interactive state: this app has no per-file review-apply logic yet, so it's deliberately
/// given no `cursor_pointer()`/`on_click` at all rather than a handler that would silently no-op.
///
/// The trailing keycap is resolved through `crate::keymap::resolve_combo("enter", macos)` rather
/// than a baked-in `⏎` glyph, so it reads `Enter` on Windows/Linux.
pub(in crate::code_surface) fn render_accept_file_button(macos: bool) -> impl IntoElement {
    let parts = keymap::resolve_combo("enter", macos);
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.0))
        .px(px(8.0))
        .py(px(3.0))
        .rounded(theme::radius::BUTTON)
        .border_1()
        .border_color(theme::border::BUTTON_DISABLED)
        .child(
            div()
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(10.5))
                .text_color(theme::text::GHOSTER)
                .child("Accept file"),
        )
        .child(render_action_keycap_row(
            &parts,
            theme::text::GHOSTER.into(),
            theme::border::BUTTON_DISABLED.into(),
        ))
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

        assert!(
            cx.debug_bounds("diff-line-299").is_some(),
            "the last row within the real render cap should have really painted"
        );
        assert!(
            cx.debug_bounds("diff-line-300").is_none(),
            "a row past the real render cap must not exist at all"
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
