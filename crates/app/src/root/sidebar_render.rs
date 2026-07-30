use super::*;
use crate::root::settings_widgets::ChoiceOption;
use crate::root::widgets::{render_sidebar_message, render_tag_pill, text_tooltip};

impl AdeApp {
    /// Switches which data source the right sidebar shows. Switching *to* the Changes view
    /// always recomputes the diff (`load_diff`, not just `cx.notify()`) rather than showing
    /// whatever was last loaded - a stale snapshot from when the worktree was first selected
    /// would silently hide changes an agent just made.
    pub(super) fn set_right_sidebar_view(
        &mut self,
        view: RightSidebarView,
        cx: &mut Context<Self>,
    ) {
        self.right_sidebar_view = view;
        if view == RightSidebarView::Changes {
            self.load_diff(self.diff_root.clone(), cx);
        } else {
            cx.notify();
        }
    }

    /// Toggles a directory's collapsed/expanded state - `crate::file_tree::visible_entries`
    /// does the actual hiding at render time.
    pub(super) fn toggle_dir_collapsed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(&path) {
            self.collapsed_dirs.insert(path);
        }
        cx.notify();
    }

    /// Toggles a file's reviewed state - the Changes row checkbox's click handler.
    /// `Self::render_change_row` stops propagation at the call site so checking a box never
    /// also opens that file's diff.
    pub(super) fn toggle_reviewed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.reviewed_files.remove(&path) {
            self.reviewed_files.insert(path);
        }
        cx.notify();
    }

    /// The `A`/`M` change marks for every changed file in the currently loaded diff, keyed by
    /// each file's absolute path. Built *once* per [`Self::render_file_tree`] call rather than
    /// the row itself re-scanning `diff.files` per row per frame: back when every one of up to
    /// 500 visible rows really was built each frame, that per-row scan against up to 300 diff
    /// files was a measured ~21ms foreground stall on a ~33ms frame budget. The row count is far
    /// smaller now that [`Self::render_file_tree`] is virtualized, so the stall this originally
    /// fixed is no longer reachable at that magnitude - but building the map once and reusing it
    /// is still both cheaper and simpler than a per-row scan, so the shape stands unchanged.
    /// A deleted file never needs an entry here:
    /// `crate::file_tree::build_file_tree` only lists currently-existing entries.
    pub(super) fn tree_change_marks(&self) -> HashMap<PathBuf, (&'static str, gpui::Rgba)> {
        let Some(diff) = self.current_diff() else {
            return HashMap::new();
        };
        diff.files
            .iter()
            .filter_map(|file| {
                let mark = match file.status {
                    FileChangeStatus::Added => ("A", theme::tag::TREE_ADDED.into()),
                    FileChangeStatus::Modified | FileChangeStatus::Renamed => {
                        ("M", theme::tag::TREE_MODIFIED.into())
                    }
                    FileChangeStatus::Deleted => return None,
                };
                Some((self.file_tree_root.join(&file.path), mark))
            })
            .collect()
    }

    /// The file tree - `design_handoff_jerry_ade/README.md`'s Zone 3 "Files (tree)" spec:
    /// rect-composed folder/language-chip icons (see [`render_folder_icon`]/
    /// [`render_lang_chip`], never emoji or an SVG pipeline), collapse/expand (see
    /// [`Self::toggle_dir_collapsed`]/`crate::file_tree::visible_entries`).
    ///
    /// **Scrolling lives here, not in the caller.** This list is a `gpui::uniform_list`, which
    /// sets its own `overflow.y = Scroll` and owns the scroll offset
    /// (`vendor/zed/crates/gpui/src/elements/uniform_list.rs`'s `uniform_list()`), so
    /// `Self::render_right_sidebar` deliberately does *not* wrap it in a second
    /// `overflow_y_scroll()` container any more. It used to, back when this was an eager
    /// `flex_col` of every row: an outer scroller plus a naturally-grown child was the only way
    /// to scroll then, and re-adding one now would let this list expand to its full virtual
    /// height inside that outer scroller, silently undoing the virtualization while still
    /// *looking* correct.
    pub(super) fn render_file_tree(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Both early returns keep their own scroll box. `Self::render_right_sidebar`'s Files arm
        // can no longer be a scroller itself (the `uniform_list` below owns scrolling), but
        // these two paths render no list at all - and a real `std::io::Error` from an
        // unreadable directory is an arbitrarily long string that used to be scrollable inside
        // that outer container. Dropping it would silently clip the very message the user needs
        // in order to understand why the tree is empty.
        if let Some(error) = &self.file_tree_error {
            return scrollable_sidebar_message(
                "file-tree-error",
                format!("failed to read directory: {error}"),
                theme::status::FAIL.into(),
            );
        }
        if self.file_tree.is_empty() {
            return scrollable_sidebar_message(
                "file-tree-empty",
                "(empty directory)".to_string(),
                theme::text::FAINT.into(),
            );
        }

        let visible_count = file_tree::visible_entries(&self.file_tree, &self.collapsed_dirs).len();
        let rendered_count = visible_count.min(file_tree::MAX_RENDERED_FILE_ENTRIES);

        // Built once per render, not once per row - see `Self::tree_change_marks`'s docs. Moved
        // *into* the row-builder closure below (rather than recomputed inside it) because
        // `uniform_list` calls that closure **three** times per frame, not once: `measure_item`
        // from `request_layout`, `measure_item` again from `prepaint`, then `render_items` for
        // the real visible range (`vendor/zed/crates/gpui/src/elements/uniform_list.rs:283`,
        // `:359`, `:489`). Capturing the map keeps it at one build per frame, exactly as the
        // previous eager loop had it. `file_tree::visible_entries` below does *not* get that
        // treatment and genuinely does run three times per frame now where it used to run once -
        // a real, accepted regression on that one call: it is a borrowing walk with a single
        // `Vec` allocation, measured at well under a millisecond against the ~145ms of per-frame
        // element layout this whole change removes, and capturing it instead would mean cloning
        // every entry (the walk borrows from `self`, so the `'static` closure cannot hold it).
        let marks = self.tree_change_marks();

        // Virtualized: only the rows genuinely on screen become real elements. Previously every
        // one of up to `file_tree::MAX_RENDERED_FILE_ENTRIES` (500) *visible* rows was built,
        // laid out and painted on every single frame - including the ~460 of them scrolled off
        // screen - which measured (real `gpui::FrameTiming` data, debug build, this repository's
        // own tree, terminal streaming) as ~145ms of a ~200ms `Window::draw`, i.e. ~72% of the
        // entire frame, holding the whole app at ~4fps. `uniform_list` is the same real
        // virtualization the File view's own code list already uses
        // (`crate::root::code_surface::AdeApp::render_file_view`,
        // `vendor/zed/crates/gpui/examples/uniform_list.rs`); every row here is exactly
        // `theme::band::TREE_ROW` tall, which is `uniform_list`'s one real requirement.
        //
        // `MAX_RENDERED_FILE_ENTRIES` deliberately stays: it is no longer a layout-cost guard
        // (virtualization removed that cost), but it is still the real bound on how much of a
        // huge tree this list claims to represent, and on the scroll extent `uniform_list`
        // derives from `item_count`. The "... and N more" trailer below keeps that honest.
        let list = uniform_list(
            "file-tree-list",
            rendered_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                // Recomputed from the same two fields `rendered_count` above was derived from,
                // which cannot change between that read and this call within one frame. The
                // range is still clamped rather than indexed blindly, so a future divergence
                // degrades to "renders fewer rows" instead of panicking.
                let visible = file_tree::visible_entries(&this.file_tree, &this.collapsed_dirs);
                let start = range.start.min(visible.len());
                let end = range.end.min(visible.len());
                visible[start..end]
                    .iter()
                    .map(|entry| this.render_file_tree_row(entry, &marks, cx))
                    .collect::<Vec<_>>()
            }),
        )
        // Load-bearing, and it fails *silently* if removed. `uniform_list`'s default
        // `sizing_behavior` is `ListSizingBehavior::Auto`, which takes the
        // `window.request_layout(style, None, cx)` branch - no children, no measure function -
        // so the element's intrinsic height is zero and every pixel of its height comes from
        // this `flex_1`. Drop it, or put this list under any ancestor without a definite
        // height, and the list renders zero rows with no panic and no warning.
        .flex_1()
        .min_h_0();

        // `flex_1().min_h_0()`, deliberately not `size_full()`. Both do in fact lay out
        // correctly here - GPUI's sizes are border-box, so an `h_full()` alongside this 4px of
        // vertical padding would *not* overflow (taffy's `Style::default()` sets
        // `box_sizing: BorderBox`, and gpui's `ToTaffy for Style`
        // (`vendor/zed/crates/gpui/src/taffy.rs`) never overrides it). `flex_1` is chosen
        // because it stays correct without depending on that: as the sole flex child it takes
        // exactly the leftover space, whatever siblings the trailer below adds.
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .flex_1()
            .min_h_0()
            .py(px(4.0))
            .child(list);
        if visible_count > rendered_count {
            column = column.child(render_sidebar_message(
                format!(
                    "... and {} more entries not shown",
                    visible_count - rendered_count
                ),
                theme::text::FAINT.into(),
            ));
        }

        column.into_any_element()
    }

    /// One file-tree row: indent (13px/level, per the README), a composed icon (a folder's
    /// two-rect glyph or a file's language chip), the name, and, for a directory, a click
    /// handler that toggles [`Self::collapsed_dirs`].
    pub(super) fn render_file_tree_row(
        &self,
        entry: &FileTreeEntry,
        marks: &HashMap<PathBuf, (&'static str, gpui::Rgba)>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let indent = px(13.0 * entry.depth as f32);
        let is_open = entry.is_dir && !self.collapsed_dirs.contains(&entry.path);
        let mark = marks.get(&entry.path).copied();
        // The Files tree's row-selection highlight (README's Zone 3 "Selected row bg
        // `#1a1e21`") - set by `Self::open_file_view` (this row's own click handler, below)
        // and by `Self::open_palette_file_result` for a palette file result with no diff.
        let is_selected = self.selected_tree_path.as_deref() == Some(entry.path.as_path());

        // `debug_selector` is a no-op outside test builds; lets a real render test assert on
        // which rows this list genuinely painted, which is the only way to prove the
        // `uniform_list` in `Self::render_file_tree` really is virtualizing (a row far below the
        // viewport must *not* paint) rather than just looking like it should.
        //
        // The closure *borrows* `entry` rather than capturing an owned clone of its name. That
        // matters: only the `debug_selector` method itself is `cfg`'d away in a release build
        // (`vendor/zed/crates/gpui/src/elements/div.rs`), never its argument - so a `let name =
        // entry.name.clone()` above this would be a real, test-only allocation paid on every
        // visible row on every frame in release. `debug_selector` puts no `'static` bound on the
        // closure and calls it immediately, so borrowing is sound, and the `format!` never runs
        // at all outside test builds.
        let mut row = div()
            .id(format!("file-tree-row-{}", entry.path.display()))
            .debug_selector(|| format!("file-tree-row-{}", entry.name))
            .flex()
            .w_full()
            .items_center()
            .gap(px(6.0))
            .h(theme::band::TREE_ROW)
            .pl(px(8.0) + indent)
            .pr(px(8.0))
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(11.5))
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED));

        if entry.is_dir {
            let path = entry.path.clone();
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.toggle_dir_collapsed(path.clone(), cx);
                }));
        } else {
            // Opens the file in Surface C's File view - see `Self::open_file_view`'s docs.
            let path = entry.path.clone();
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.open_file_view(path.clone(), window, cx);
                }));
        }

        row = row
            .child(render_tree_caret(
                entry.is_dir,
                is_open,
                self.ui_text_size(9.0),
            ))
            .child(if entry.is_dir {
                render_folder_icon(is_open).into_any_element()
            } else {
                render_lang_chip(file_tree::lang_chip_for_name(&entry.name)).into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(if entry.is_dir {
                        theme::text::SECONDARY
                    } else {
                        theme::text::STRONG
                    })
                    .child(entry.name.clone()),
            );

        if let Some((label, color)) = mark {
            row = row.child(
                div()
                    .flex_none()
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(color)
                    .child(label),
            );
        }

        // A real, always-present (deliberately not hover-only - this project has no established
        // "hidden until row hover" mechanism yet, and a subtle-but-always-there affordance beats
        // an invented one) "new file in this directory" control - the file tree's own equivalent
        // of a right-click "New file" context menu item, since this app has no context-menu
        // mechanism anywhere yet either. Stops propagation so it never also toggles the
        // directory's own collapse state (its row's own `on_click`, registered above).
        if entry.is_dir {
            let parent_dir = entry.path.clone();
            row = row.child(
                div()
                    .id(format!("file-tree-new-file-{}", entry.path.display()))
                    .flex_none()
                    .cursor_pointer()
                    .px(px(4.0))
                    .rounded(theme::radius::CHIP)
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(11.0))
                    .text_color(theme::text::GHOST)
                    .hover(|el| {
                        el.bg(theme::surface::ROW_HOVER_ALT)
                            .text_color(theme::text::PRIMARY)
                    })
                    .tooltip(text_tooltip(format!("New file in {}", entry.name.as_str())))
                    .child("+")
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.start_new_file(parent_dir.clone(), window, cx);
                    })),
            );
        }

        row.into_any_element()
    }

    /// Zone 3's header band (36 high): the real `Files | Changes` segmented control
    /// (`design_handoff_jerry_ade/README.md`: "Header 36: segmented `Files | Changes`
    /// (Files is first and default...)") plus the real `+n`/`−n` totals across the currently
    /// loaded diff, summed from the same real per-file stats
    /// (`crate::changes::diff_file_stats`) the Changes rows themselves show.
    pub(super) fn render_right_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = match self.right_sidebar_view {
            RightSidebarView::Files => "Files",
            RightSidebarView::Changes => "Changes",
        };
        let toggle = self.render_choice_control(
            "right-sidebar-toggle",
            &[ChoiceOption::new("Files"), ChoiceOption::new("Changes")],
            selected.to_string(),
            cx,
            |this, index, cx| {
                // Structural, not a label re-match: index 0 is `Files`, index 1 is `Changes`,
                // per the `options` array literal right above - see
                // `Self::render_choice_control`'s own docs for why dispatch is index-based.
                let view = match index {
                    1 => RightSidebarView::Changes,
                    _ => RightSidebarView::Files,
                };
                this.set_right_sidebar_view(view, cx);
            },
        );

        let totals = self.diff_totals;

        div()
            .flex_none()
            .h(theme::band::PANEL_HEADER)
            .flex()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(toggle)
            .when_some(totals, |el, (add, del)| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(10.0))
                        .child(
                            div()
                                .text_color(theme::diff::STAT_ADD)
                                .child(format!("+{add}")),
                        )
                        .child(
                            div()
                                .text_color(theme::diff::STAT_DEL)
                                .child(format!("\u{2212}{del}")),
                        ),
                )
            })
            // Root-level "New file" - creates directly in the worktree root, the one location
            // the per-directory "+" on `Self::render_file_tree_row` can't reach (the root itself
            // has no row of its own to attach to). Only shown for the Files view - the Changes
            // list has no directory concept to anchor a "new file" affordance to.
            .when(self.right_sidebar_view == RightSidebarView::Files, |el| {
                let root = self.file_tree_root.clone();
                el.child(
                    div()
                        .id("file-tree-new-file-root")
                        .flex_none()
                        .cursor_pointer()
                        .px(px(5.0))
                        .rounded(theme::radius::CHIP)
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(12.0))
                        .text_color(theme::text::GHOST)
                        .hover(|el| {
                            el.bg(theme::surface::ROW_HOVER_ALT)
                                .text_color(theme::text::PRIMARY)
                        })
                        .tooltip(text_tooltip("New file in worktree root"))
                        .child("+")
                        .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                            this.start_new_file(root.clone(), window, cx);
                        })),
                )
            })
    }

    /// Zone 3's whole real body: the `Files | Changes` header, then either the scrollable file
    /// tree, or the Changes list's own header/scrollable-rows/footer trio -
    /// `design_handoff_jerry_ade/README.md`'s Changes spec ("Header 7/12 ... Footer 29"). Both
    /// list arms wrap their list in a plain `flex_1().min_h_0()` column, so a long list scrolls
    /// under its own pinned header/footer instead of pushing them off-screen.
    ///
    /// The scrolling itself belongs to the list, not to that wrapper - see
    /// [`Self::render_file_tree`]'s docs for why re-adding an `overflow_y_scroll()` here would
    /// silently undo the virtualization. Only the two *message-only* arms (no list at all) are
    /// scrollers in their own right; [`scrollable_sidebar_message`] covers the equivalent cases
    /// inside [`Self::render_file_tree`].
    pub(super) fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let container = div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_right_sidebar_toggle(cx));

        match self.right_sidebar_view {
            // Deliberately *not* `.overflow_y_scroll()` any more: `Self::render_file_tree`'s
            // `uniform_list` sets its own `overflow.y = Scroll` and owns the scroll offset
            // (`vendor/zed/crates/gpui/src/elements/uniform_list.rs`'s `uniform_list()`), so an
            // outer scroll box here would let the list grow to its full virtual height inside a
            // second scroller and defeat the virtualization entirely.
            RightSidebarView::Files => container.child(
                div()
                    .id("right-sidebar-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_file_tree(cx)),
            ),
            RightSidebarView::Changes => match self.current_diff() {
                Some(diff) => {
                    let header = self.render_changes_header(diff);
                    container
                        .child(header)
                        // Not `.overflow_y_scroll()` - see the Files arm's own comment above;
                        // `Self::render_changes_rows`'s `uniform_list` owns its own scrolling.
                        .child(
                            div()
                                .id("right-sidebar-body")
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_h_0()
                                .child(self.render_changes_rows(cx)),
                        )
                        .child(render_changes_footer(self.ui_text_size(10.0)))
                }
                // This arm keeps its `.overflow_y_scroll()`: it renders a single message, never
                // a `uniform_list`, and `Self::render_diff_state_message`'s "failed to compute
                // diff: {err}" carries an arbitrarily long real error.
                None => container.child(
                    div()
                        .id("right-sidebar-body")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .child(self.render_diff_state_message()),
                ),
            },
        }
    }

    /// The Changes header: file count, a review-progress bar, and `N reviewed` count, both
    /// computed directly from [`Self::reviewed_files`]'s membership against `diff`'s file
    /// list rather than an independently tracked counter that could drift.
    pub(super) fn render_changes_header(&self, diff: &WorktreeDiff) -> impl IntoElement {
        let total = diff.files.len();
        let reviewed = diff
            .files
            .iter()
            .filter(|file| self.reviewed_files.contains(&file.path))
            .count();
        let progress = changes::ReviewProgress { reviewed, total };
        let fraction = progress.fraction();
        const BAR_WIDTH: f32 = 56.0;

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(7.0))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::DIM)
                    .child(format!("{total} file{}", if total == 1 { "" } else { "s" })),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(BAR_WIDTH))
                    .h(px(3.0))
                    .rounded(px(1.5))
                    .bg(theme::diff::STAT_EMPTY)
                    .child(
                        div()
                            .absolute()
                            .left(px(0.0))
                            .top(px(0.0))
                            .h(px(3.0))
                            .w(px(BAR_WIDTH * fraction))
                            .rounded(px(1.5))
                            .bg(theme::status::REVIEW),
                    ),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::DIM)
                    .child(format!("{reviewed} reviewed")),
            )
    }

    /// The Changes list's scrollable rows - falls back to [`Self::render_diff_state_message`]
    /// if the diff isn't loaded, or a "no changes" message for a clean worktree.
    pub(super) fn render_changes_rows(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(diff) = self.current_diff() else {
            // Defensive: `Self::render_right_sidebar`'s Changes arm already matched
            // `Some(diff)` before calling this. Kept scrollable anyway for the same reason
            // that arm's own `None` branch is - this returns a real, unbounded error string.
            return div()
                .id("changes-state-message")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(self.render_diff_state_message())
                .into_any_element();
        };
        if diff.files.is_empty() {
            return render_sidebar_message("no changes".to_string(), theme::text::FAINT.into());
        }

        let total_files = diff.files.len();
        let truncated = diff.truncated;
        let rendered_count = total_files.min(MAX_RENDERED_DIFF_FILES);

        // `flex_1().min_h_0()` rather than `size_full()` - see `Self::render_file_tree`'s own
        // comment on the same choice.
        let mut column = div()
            .id("changes-rows")
            .flex()
            .flex_col()
            .w_full()
            .flex_1()
            .min_h_0();
        // `diff.truncated` is `wt_core::diff`'s own load-time cap firing (2MB of raw `git diff`
        // output, or more than 300 changed files) - distinct from a single file's own
        // `DiffFile::truncated` (per-file hunk-line cap, surfaced in
        // `Self::render_diff_file_detail`) and this list's own `MAX_RENDERED_DIFF_FILES`
        // *render* cap below, which only ever omits already fully-loaded data.
        //
        // A sibling of the list rather than its first child, now that the rows are a
        // `uniform_list`: it is not a `theme::band::CHANGE_ROW`-tall row, and `uniform_list`
        // sizes every slot from item 0 alone.
        if truncated {
            column = column.child(render_sidebar_message(
                "diff truncated: this worktree's real changes exceeded wt_core::diff's own \
                 load limits, so some files or lines are missing from this list"
                    .to_string(),
                theme::status::ASK.into(),
            ));
        }

        // Virtualized for the same measured reason as `Self::render_file_tree` - see that
        // method's own docs. Up to `MAX_RENDERED_DIFF_FILES` change rows were previously built,
        // laid out and painted every frame regardless of how few were on screen; every row is
        // exactly `theme::band::CHANGE_ROW` tall, which is `uniform_list`'s one real requirement.
        column = column.child(
            uniform_list(
                "changes-rows-list",
                rendered_count,
                cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                    // Re-resolved (not captured) so a diff that got replaced between this
                    // frame's `item_count` read and this call renders fewer rows rather than
                    // indexing a stale snapshot.
                    let Some(diff) = this.current_diff() else {
                        return Vec::new();
                    };
                    let start = range.start.min(diff.files.len());
                    let end = range.end.min(diff.files.len());
                    diff.files[start..end]
                        .iter()
                        .map(|file| this.render_change_row(file, cx).into_any_element())
                        .collect::<Vec<_>>()
                }),
            )
            .flex_1()
            .min_h_0(),
        );

        if total_files > rendered_count {
            column = column.child(render_sidebar_message(
                format!(
                    "... and {} more changed files not shown",
                    total_files - rendered_count
                ),
                theme::text::FAINT.into(),
            ));
        }
        column.into_any_element()
    }

    /// One Changes row: a review checkbox, `dir`/`name`, an optional tag pill, `+n`/`−n`, and
    /// the five-segment stat bar. Clicking anywhere on the row other than the checkbox itself
    /// (see [`Self::render_review_checkbox`]'s `stop_propagation`) opens the file's diff via
    /// [`Self::open_change_diff`].
    pub(super) fn render_change_row(
        &self,
        file: &DiffFile,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = file.path.clone();
        let open_path = path.clone();
        let reviewed = self.reviewed_files.contains(&file.path);
        let selected = self.open_change.as_deref() == Some(file.path.as_path());
        let (add, del) = changes::diff_file_stats(file);
        let (dir, name) = changes::split_dir_name(&file.path);
        let tag = changes::change_tag(file.status);
        let segments = changes::stat_bar_segments(add, del);

        // See `Self::render_file_tree_row`'s own `debug_selector` for why this exists, and why
        // the closure borrows `file` instead of capturing an owned `String`.
        div()
            .id(format!("change-row-{}", file.path.display()))
            .debug_selector(|| format!("change-row-{}", file.path.display()))
            .flex()
            .w_full()
            .items_center()
            .gap(px(6.0))
            .h(theme::band::CHANGE_ROW)
            .pl(px(9.0))
            .pr(px(10.0))
            .border_b_1()
            .border_color(theme::border::ROW)
            .cursor_pointer()
            .when(selected, |el| {
                el.bg(theme::surface::ROW_SELECTED)
                    .border_l_2()
                    .border_color(theme::border::SELECTED_EDGE)
            })
            .when(!selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.open_change_diff(open_path.clone(), window, cx);
            }))
            .child(self.render_review_checkbox(path, reviewed, cx))
            .when(!dir.is_empty(), |el| {
                el.child(
                    div()
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(10.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("{dir}/")),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(11.5))
                    .text_color(if reviewed {
                        theme::text::DIMMER
                    } else {
                        theme::text::STRONG
                    })
                    .child(name),
            )
            .when_some(tag, |el, tag| el.child(render_tag_pill(tag)))
            // A rename-only file gets no `tag` from `changes::change_tag` (a plain rename
            // isn't `new`/`del`), so without this it looked identical to an unchanged file.
            // `changes::is_real_rename` only fires when `old_path` differs from the current path.
            .when(changes::is_real_rename(file), |el| {
                el.child(render_moved_tag())
            })
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::diff::STAT_ADD)
                    .child(format!("+{add}")),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::diff::STAT_DEL)
                    .child(format!("\u{2212}{del}")),
            )
            .child(render_stat_bar(segments))
    }

    /// The Changes row's 12×12 review checkbox - toggled via [`Self::toggle_reviewed`]. Stops
    /// propagation on click so checking a box never also opens the row's diff, mirroring
    /// `Self::render_session_tab`'s nested-clickable-child pattern (its tab-close `×`).
    pub(super) fn render_review_checkbox(
        &self,
        path: PathBuf,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(format!("review-checkbox-{}", path.display()))
            .flex_none()
            .w(px(12.0))
            .h(px(12.0))
            .rounded(px(2.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .border_1()
            .when(checked, |el| {
                el.bg(theme::button::GREEN_BG)
                    .border_color(theme::toggle::TRACK_ON)
            })
            .when(!checked, |el| el.border_color(theme::border::BUTTON))
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(9.0))
            .text_color(theme::button::GREEN_FG)
            .when(checked, |el| el.child("\u{2713}"))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.toggle_reviewed(path.clone(), cx);
            }))
    }
}

/// Which data source the right sidebar currently shows for the selected worktree - Zone 3's
/// `right_pane` state (`Files | Changes`, `Files` default). The panel never shows diff
/// *content* (see [`AdeApp::open_change`]'s docs) - `Changes` is the per-file review list,
/// not a diff view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RightSidebarView {
    Files,
    Changes,
}

/// The Changes list's footer 29. The README's spec text also mentions `] next file`, dropped
/// here since `]` isn't actually bound to anything (only `secondary-n` is - see
/// `crate::default_key_bindings`); advertising a dead shortcut is worse than a shorter,
/// accurate footer.
///
/// `text_size` is the caller's already-scaled [`AdeApp::ui_text_size`] value - this free
/// function has no `&self` to call that method through, so the one caller
/// ([`AdeApp::render_right_sidebar`]) computes and passes it in.
pub(super) fn render_changes_footer(text_size: Pixels) -> impl IntoElement {
    div()
        .flex_none()
        .h(theme::band::SURFACE_FOOTER)
        .px(px(12.0))
        .flex()
        .items_center()
        .border_t_1()
        .border_color(theme::border::INNER)
        .bg(theme::surface::FOOTER)
        .font(font(theme::font::MONO))
        .text_size(text_size)
        .text_color(theme::text::HINT)
        .child("click a file to open its diff in the centre")
}

/// The file tree row's `▾`/`▸` caret, signaling a directory row is clickable/expandable,
/// distinct from the folder icon itself. Blank but still 8px wide for a file row, to keep
/// every row's icon column aligned.
///
/// `text_size` - see [`render_changes_footer`]'s docs for why this takes an already-scaled
/// value rather than computing it internally.
pub(super) fn render_tree_caret(is_dir: bool, open: bool, text_size: Pixels) -> impl IntoElement {
    let label = if !is_dir {
        ""
    } else if open {
        "\u{25be}"
    } else {
        "\u{25b8}"
    };
    div()
        .flex_none()
        .w(px(8.0))
        .font(font(theme::font::MONO))
        .text_size(text_size)
        .text_color(theme::text::TREE_CARET)
        .child(label)
}

/// The file tree's folder icon - two rects, a 5×3 tab and a 12×8 radius-2 body, composed
/// entirely from `div()`s (never an emoji glyph, which is what caused the "tofu box" bug:
/// no matching glyph installed on the reporting machine).
///
/// The two rects are *not* styled identically (verified against `design_handoff_jerry_ade/
/// Jerry.dc.html`'s `n.folderBd`/`n.folderBg`): the body alternates between a filled `bg`
/// (open) and transparent (collapsed), both with a `border` - but the tab is always
/// solid-filled with the `border` colour and has no separate border of its own. An earlier
/// version gave the tab the same hollow-when-collapsed treatment as the body; the mockup's
/// collapsed-folder tab is solid, not outlined.
pub(super) fn render_folder_icon(open: bool) -> impl IntoElement {
    let (fill, border): (gpui::Rgba, gpui::Rgba) = if open {
        (
            theme::surface::CHIP_NEUTRAL.into(),
            theme::text::FAINT.into(),
        )
    } else {
        (work_surface::TRANSPARENT, theme::text::GHOST.into())
    };

    div()
        .relative()
        .flex_none()
        .w(px(12.0))
        .h(px(11.0))
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(1.0))
                .w(px(5.0))
                .h(px(3.0))
                .bg(border),
        )
        .child(
            div()
                .absolute()
                .left(px(0.0))
                .top(px(3.0))
                .w(px(12.0))
                .h(px(8.0))
                .rounded(px(2.0))
                .bg(fill)
                .border_1()
                .border_color(border),
        )
}

/// The file tree's 13×13 radius-2.5 language chip - a rect with a text-glyph label, per
/// `crate::file_tree::lang_chip_for_name`'s selection logic (never an emoji, never a second,
/// independently maintained extension-matching guess).
pub(super) fn render_lang_chip(chip: LangChip) -> impl IntoElement {
    div()
        .flex_none()
        .w(px(13.0))
        .h(px(13.0))
        .rounded(px(2.5))
        .bg(chip.bg)
        .flex()
        .items_center()
        .justify_center()
        .font(font(theme::font::MONO))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(7.5))
        .text_color(chip.fg)
        .child(chip.label)
}

/// [`render_sidebar_message`] inside its own scroll box, for the right sidebar's message-only
/// states (an unreadable directory, an empty tree, a diff that failed to compute).
///
/// These used to inherit scrolling from [`AdeApp::render_right_sidebar`]'s own
/// `overflow_y_scroll()` container. That container had to go for the Files/Changes list arms -
/// a `gpui::uniform_list` owns its own scrolling and would expand to its full virtual height
/// inside an outer scroller - but these paths render no list at all, and the messages they show
/// wrap a real, arbitrarily long `std::io::Error`/`git` error. Without this, a long error would
/// be silently clipped at the panel's bottom edge with no way to read the rest of it.
pub(super) fn scrollable_sidebar_message(
    id: &'static str,
    text: String,
    color: gpui::Rgba,
) -> gpui::AnyElement {
    div()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(render_sidebar_message(text, color))
        .into_any_element()
}

/// The Changes row's `moved` tag for a real rename (`changes::is_real_rename`) - its own
/// muted style rather than [`ChangeTag`]'s bg/fg pair, since that enum only covers
/// `new`/`del` and reusing an unrelated colour for a third meaning seemed worse than a plain
/// neutral tag.
pub(super) fn render_moved_tag() -> impl IntoElement {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(1.0))
        .rounded(theme::radius::CHIP)
        .bg(theme::surface::CHIP_NEUTRAL)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(theme::text::GHOST)
        .child("moved")
}

/// The Changes row's real five-segment 3×8 stat bar (`design_handoff_jerry_ade/README.md`:
/// "a five-segment 3×8 stat bar (`#4e8c68` / `#a35f5b` / `#22262a`)"), per
/// `crate::changes::stat_bar_segments`'s real, unit-tested proportional allocation.
pub(super) fn render_stat_bar(segments: [changes::StatSegment; 5]) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .gap(px(1.0))
        .children(segments.into_iter().map(|segment| {
            div()
                .w(px(3.0))
                .h(px(8.0))
                .bg(changes::stat_segment_color(segment))
        }))
}

/// Real, live-rendered proof that the right sidebar's two long lists are genuinely virtualized -
/// that is, that a row scrolled far below the viewport is not merely *invisible* but never
/// becomes a painted element at all.
///
/// This is the property the whole fix rests on, and it is not observable from the pure logic:
/// `crate::file_tree::visible_entries` reports exactly the same rows either way. Only a real
/// render can tell "built 500 elements and clipped 460 of them" apart from "built 40". Both
/// tests therefore also assert the *positive* half - that the rows which should paint really do -
/// so a future change that virtualizes by simply rendering nothing would fail here rather than
/// pass.
#[cfg(test)]
mod virtualization_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::fs;
    use std::process::Command;
    use tempfile::TempDir;

    /// `.output()`, not `.status()`: `status()` inherits stdout/stderr, so seeding a 40-file
    /// repository below dumped forty `create mode 100644 f-NN.txt` lines into the test output.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Before this revision's fix, every one of the first
    /// `file_tree::MAX_RENDERED_FILE_ENTRIES` (500) visible rows was built, laid out and painted
    /// on *every* frame - including all the ones below the fold - which measured, against real
    /// `gpui::FrameTiming` data on this repository's own tree, as ~145ms of a ~200ms
    /// `Window::draw`.
    #[gpui::test]
    fn a_file_tree_row_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        // Deliberately more rows than any plausible test viewport can show at
        // `theme::band::TREE_ROW` (22px) each, but fewer than
        // `file_tree::MAX_RENDERED_FILE_ENTRIES`, so this measures virtualization alone and not
        // the "... and N more entries not shown" cap.
        for index in 0..300 {
            fs::write(repo.path().join(format!("file-{index:03}.txt")), "x\n").expect("write");
        }
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-row-file-000.txt").is_some(),
            "the first file-tree row must really paint - if it doesn't, this test proves \
             nothing about virtualization, only that the tree is empty"
        );
        assert!(
            cx.debug_bounds("file-tree-row-file-299.txt").is_none(),
            "the 300th file-tree row is far below any plausible viewport, so a virtualized \
             list must never build it as an element at all"
        );
    }

    /// The other half of "is it really virtualized": a row that legitimately isn't painted yet
    /// must still be reachable. This scrolls the real list with a real
    /// `gpui::ScrollWheelEvent` and asserts the row that was previously absent genuinely
    /// materializes - which simultaneously proves the list still scrolls at all after
    /// `Self::render_right_sidebar` stopped wrapping it in its own `overflow_y_scroll()`
    /// container, the one behaviour that change could plausibly have broken.
    #[gpui::test]
    fn scrolling_the_virtualized_file_tree_materializes_a_row_that_was_not_painted(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        for index in 0..300 {
            fs::write(repo.path().join(format!("file-{index:03}.txt")), "x\n").expect("write");
        }
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let first_row = cx
            .debug_bounds("file-tree-row-file-000.txt")
            .expect("the first file-tree row must really paint");
        assert!(
            cx.debug_bounds("file-tree-row-file-299.txt").is_none(),
            "precondition: the last row must not be painted before scrolling"
        );

        // A deliberately huge delta: `uniform_list` clamps to its own real maximum scroll
        // offset, so this lands at the true bottom of the list without this test having to
        // model row heights or viewport size itself.
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: first_row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-row-file-299.txt").is_some(),
            "scrolling to the bottom must really materialize the last row - if this fails the \
             list is not scrollable any more, which is a far worse regression than the render \
             cost this change set out to fix"
        );
    }

    /// The correctness half of the same change: virtualizing must not break the tree's real
    /// content. A collapsed directory's children must still be genuinely absent, and expanding
    /// it must genuinely bring them back - the state `crate::file_tree::visible_entries` owns,
    /// now consulted from inside `uniform_list`'s row-builder rather than from an eager loop.
    ///
    /// Honest about its own reach: with only two entries this exercises no virtualization at
    /// all, and it passes identically against the pre-fix eager loop. It is a guard on the
    /// tree's *content* surviving the rewrite, not evidence that the rewrite virtualizes -
    /// that is what the two "far below the viewport" tests and the scroll test are for.
    #[gpui::test]
    fn collapsing_a_directory_still_removes_its_children_from_the_virtualized_tree(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir(repo.path().join("src")).expect("mkdir");
        fs::write(repo.path().join("src/only.rs"), "fn main() {}\n").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-row-only.rs").is_some(),
            "an expanded directory's child must really paint"
        );

        let src_dir = repo.path().join("src");
        app.update(cx, |app, cx| {
            app.toggle_dir_collapsed(src_dir.clone(), cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-row-only.rs").is_none(),
            "a collapsed directory's child must not paint"
        );

        app.update(cx, |app, cx| {
            app.toggle_dir_collapsed(src_dir, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-row-only.rs").is_some(),
            "re-expanding must bring the child back - a virtualized list that caches its row \
             set without invalidating on `collapsed_dirs` would fail exactly here"
        );
    }

    /// The Changes list got the same treatment, and needs the same proof.
    ///
    /// The margin here is real but thinner than it looks, so it is worth stating rather than
    /// implying: the test display is 1920x1080
    /// (`vendor/zed/crates/gpui/src/platform/test/display.rs`), and `MAX_RENDERED_DIFF_FILES`
    /// (40) rows at `theme::band::CHANGE_ROW` (27px) is exactly 1080px. What puts the last row
    /// off screen is the ~159px of real window chrome above and below it (title bar, panel
    /// header, Changes header, footer, status bar) - about five rows' worth. If that chrome ever
    /// shrinks substantially this test fails loudly rather than silently passing for the wrong
    /// reason, but it is not the comfortable margin a bigger row count would buy.
    #[gpui::test]
    fn a_changes_row_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        for index in 0..40 {
            fs::write(repo.path().join(format!("f-{index:02}.txt")), "base\n").expect("write");
        }
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        // On the default branch there is no base to diff against
        // (`wt_core::diff::DiffBase::OnDefaultBranch`), so this has to be a real feature branch
        // for `AdeApp::current_diff` to ever be `Some` - the same setup this crate's existing
        // real-diff tests use.
        git(repo.path(), &["checkout", "-b", "feature"]);
        for index in 0..40 {
            fs::write(
                repo.path().join(format!("f-{index:02}.txt")),
                "base\nchanged\n",
            )
            .expect("write");
        }

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.current_diff().map(|d| d.files.len())),
            Some(40),
            "sanity check: all 40 files must really be in the loaded diff, otherwise the \
             assertions below would pass for the wrong reason"
        );
        assert!(
            cx.debug_bounds("change-row-f-00.txt").is_some(),
            "the first changed file's row must really paint - otherwise this test proves \
             nothing about virtualization, only that the diff never loaded"
        );
        assert!(
            cx.debug_bounds("change-row-f-39.txt").is_none(),
            "the 40th changed file's row is past any plausible viewport, so a virtualized \
             list must never build it as an element at all"
        );
    }
}
