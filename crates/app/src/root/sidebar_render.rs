use super::*;
use crate::root::widgets::{render_sidebar_message, render_tag_pill};

impl AdeApp {
    /// Switches which real data source the right sidebar shows. Switching *to* the Changes
    /// view always recomputes the diff (`load_diff`, not just `cx.notify()`) rather than
    /// showing whatever was last loaded: the core workflow this feature exists for is "run an
    /// agent in a terminal tab, then check what changed", and a stale snapshot captured back
    /// when the worktree was first selected would silently hide exactly the changes just made -
    /// worse than an obviously-loading state.
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

    /// Toggles a directory's collapsed/expanded state - the file tree row's real click handler
    /// (`crate::file_tree::visible_entries` does the actual hiding at render time).
    pub(super) fn toggle_dir_collapsed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(&path) {
            self.collapsed_dirs.insert(path);
        }
        cx.notify();
    }

    /// Toggles a file's real reviewed/not-reviewed state - the Changes row checkbox's click
    /// handler. Deliberately stops propagation at the call site (see
    /// `Self::render_change_row`) so checking a box never also opens that file's diff.
    pub(super) fn toggle_reviewed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if !self.reviewed_files.remove(&path) {
            self.reviewed_files.insert(path);
        }
        cx.notify();
    }

    /// The real `A`/`M` change marks for every changed file in the currently loaded diff, keyed
    /// by each file's absolute path (`design_handoff_jerry_ade/README.md`: "Changed files carry
    /// an `A` ... or `M` ... mark at the right"). Built *once* per [`Self::render_file_tree`]
    /// call and looked up per row from there, rather than the row itself re-scanning
    /// `diff.files` (a `Vec`, so a linear scan) and re-joining a `PathBuf` for every rendered
    /// row: with up to `file_tree::MAX_RENDERED_FILE_ENTRIES` (500) rows rendered against up to
    /// 300 loaded diff files, doing that scan+allocation per row per frame was a real, measured
    /// ~21ms foreground-executor cost against a ~33ms frame budget - exactly the kind of stall
    /// `file_tree::MAX_RENDERED_FILE_ENTRIES`'s own doc comment already warned a prior phase
    /// measured. A
    /// deleted file never needs an entry here: `crate::file_tree::build_file_tree` only ever
    /// lists real, currently-existing directory entries, so a deleted path simply never produces
    /// a row to mark in the first place.
    pub(super) fn tree_change_marks(&self) -> HashMap<PathBuf, (&'static str, gpui::Rgba)> {
        let Some(diff) = self.current_diff() else {
            return HashMap::new();
        };
        diff.files
            .iter()
            .filter_map(|file| {
                let mark = match file.status {
                    FileChangeStatus::Added => ("A", theme::tag::TREE_ADDED),
                    FileChangeStatus::Modified | FileChangeStatus::Renamed => {
                        ("M", theme::tag::TREE_MODIFIED)
                    }
                    FileChangeStatus::Deleted => return None,
                };
                Some((self.file_tree_root.join(&file.path), mark))
            })
            .collect()
    }

    /// The real file tree - `design_handoff_jerry_ade/README.md`'s Zone 3 "Files (tree)" spec:
    /// real rect-composed folder/language-chip icons (see [`render_folder_icon`]/
    /// [`render_lang_chip`], never emoji or an SVG pipeline), real collapse/expand (see
    /// [`Self::toggle_dir_collapsed`]/`crate::file_tree::visible_entries`), and - critically for
    /// scrolling to actually work - **no `size_full()`/fixed height on this list**: its caller
    /// (`Self::render_right_sidebar`) puts it inside a `flex_1().min_h_0().overflow_y_scroll()`
    /// wrapper, and a scrollable container's child must be free to grow to its own natural
    /// content height (not clamped to `height: 100%` of the scroll viewport) for there to be
    /// anything to scroll *to*. That `size_full()` on this method's own root div was exactly
    /// the reported "file tree scroll doesn't work" bug: it pinned the list's height to the
    /// visible viewport regardless of how many rows it held, so content past the bottom was
    /// silently clipped instead of scrollable (verified against `Self::render_rail_list`, which
    /// never had this bug - it was never given a `size_full()` in the first place).
    pub(super) fn render_file_tree(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(error) = &self.file_tree_error {
            return render_sidebar_message(
                format!("failed to read directory: {error}"),
                theme::status::FAIL,
            );
        }
        if self.file_tree.is_empty() {
            return render_sidebar_message("(empty directory)".to_string(), theme::text::FAINT);
        }

        let visible = file_tree::visible_entries(&self.file_tree, &self.collapsed_dirs);

        // Only the first `file_tree::MAX_RENDERED_FILE_ENTRIES` *visible* rows are turned into
        // actual elements - independent of `self.file_tree`'s own up-to-`file_tree::MAX_ENTRIES`
        // (5000) loaded size. Laying out that many `div`s through GPUI's flexbox engine on
        // *every* render (which happens as often as every ~33ms while a terminal pane is
        // streaming output and calling `cx.notify()`) was a real, measured foreground-executor
        // stall during an earlier step's own verification (see this constant's docs) - a real
        // virtualized list (`uniform_list`, see `vendor/zed/crates/project_panel`) would be a
        // further improvement for a tree of unbounded size, but is out of scope here.
        let rendered_count = visible.len().min(file_tree::MAX_RENDERED_FILE_ENTRIES);

        // Built once per render, not once per row - see `Self::tree_change_marks`'s docs for
        // the real per-frame cost this avoids.
        let marks = self.tree_change_marks();

        let mut list = div().id("file-tree-list").flex().flex_col().py(px(4.0));
        for entry in &visible[..rendered_count] {
            list = list.child(self.render_file_tree_row(entry, &marks, cx));
        }
        if visible.len() > rendered_count {
            list = list.child(render_sidebar_message(
                format!(
                    "... and {} more entries not shown",
                    visible.len() - rendered_count
                ),
                theme::text::FAINT,
            ));
        }

        list.into_any_element()
    }

    /// One file-tree row: real indent (13px/level, per the README), a real composed icon (a
    /// folder's two-rect glyph or a file's language chip), the real name, and, for a directory,
    /// a real click handler that toggles [`Self::collapsed_dirs`] (never an always-expanded
    /// tree: this was the other half of the reported "collapse doesn't work" bug, since there
    /// was previously no collapse *state* at all, so every directory rendered permanently open).
    pub(super) fn render_file_tree_row(
        &self,
        entry: &FileTreeEntry,
        marks: &HashMap<PathBuf, (&'static str, gpui::Rgba)>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let indent = px(13.0 * entry.depth as f32);
        let is_open = entry.is_dir && !self.collapsed_dirs.contains(&entry.path);
        let mark = marks.get(&entry.path).copied();
        // The Files tree's own real row-selection highlight (`design_handoff_jerry_ade/
        // README.md`'s Zone 3 "Selected row bg `#1a1e21`") - only ever set by
        // `Self::open_palette_file_result` for a file result with no diff to open in the centre
        // (see that method's docs); Phase D never gave individual file rows a click handler of
        // their own, so this was previously always `false`.
        let is_selected = self.selected_tree_path.as_deref() == Some(entry.path.as_path());

        let mut row = div()
            .id(format!("file-tree-row-{}", entry.path.display()))
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(theme::band::TREE_ROW)
            .pl(px(8.0) + indent)
            .pr(px(8.0))
            .font(font(theme::font::MONO))
            .text_size(px(11.5))
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
            // A file row's own real click handler - opens it in Surface C's real File view
            // (`Self::open_file_view`'s docs explain why this, rather than the diff surface, is
            // this phase's chosen trigger for a plain Files-tree row).
            let path = entry.path.clone();
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.open_file_view(path.clone(), window, cx);
                }));
        }

        row = row
            .child(render_tree_caret(entry.is_dir, is_open))
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

        row.into_any_element()
    }

    /// Zone 3's header band (36 high): the real `Files | Changes` segmented control
    /// (`design_handoff_jerry_ade/README.md`: "Header 36: segmented `Files | Changes`
    /// (Files is first and default...)") plus the real `+n`/`−n` totals across the currently
    /// loaded diff, summed from the same real per-file stats
    /// (`crate::changes::diff_file_stats`) the Changes rows themselves show.
    pub(super) fn render_right_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let segment = |label: &'static str, view: RightSidebarView| {
            let is_active = self.right_sidebar_view == view;
            div()
                .id(label)
                .cursor_pointer()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(theme::radius::CHIP)
                .when(is_active, |el| el.bg(theme::surface::SEGMENT_ACTIVE))
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(10.5))
                .text_color(if is_active {
                    theme::text::PRIMARY
                } else {
                    theme::text::DIMMER
                })
                .child(label)
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.set_right_sidebar_view(view, cx);
                }))
        };

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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .p(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::surface::SEGMENT_TRACK)
                    .child(segment("Files", RightSidebarView::Files))
                    .child(segment("Changes", RightSidebarView::Changes)),
            )
            .when_some(totals, |el, (add, del)| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
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
    }

    /// Zone 3's whole real body: the `Files | Changes` header, then either the scrollable file
    /// tree, or the Changes list's own header/scrollable-rows/footer trio -
    /// `design_handoff_jerry_ade/README.md`'s Changes spec ("Header 7/12 ... Footer 29"), with
    /// the same `flex_1().min_h_0().overflow_y_scroll()` real-scroll wrapper
    /// [`Self::render_file_tree`]'s docs explain, so a long Changes list scrolls under its own
    /// pinned header/footer instead of pushing them off-screen.
    pub(super) fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let container = div()
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_right_sidebar_toggle(cx));

        match self.right_sidebar_view {
            RightSidebarView::Files => container.child(
                div()
                    .id("right-sidebar-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_file_tree(cx)),
            ),
            RightSidebarView::Changes => match self.current_diff() {
                Some(diff) => {
                    let header = self.render_changes_header(diff);
                    container
                        .child(header)
                        .child(
                            div()
                                .id("right-sidebar-body")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .child(self.render_changes_rows(cx)),
                        )
                        .child(render_changes_footer())
                }
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

    /// The Changes header: real file count, a real review-progress bar and `N reviewed` count
    /// (`design_handoff_jerry_ade/README.md`: "file count, a 56×3 review progress bar, and
    /// `3 reviewed`"), both computed directly from [`Self::reviewed_files`]'s real membership
    /// against `diff`'s real file list, never an independently tracked counter that could drift
    /// from what's actually checked.
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
                    .text_size(px(10.0))
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
                    .text_size(px(10.0))
                    .text_color(theme::text::DIM)
                    .child(format!("{reviewed} reviewed")),
            )
    }

    /// The Changes list's real, scrollable rows - falls back to [`Self::render_diff_state_message`]
    /// if the diff isn't loaded (defensive: `Self::render_right_sidebar` already branches on
    /// `current_diff()` before calling this, but this stays correct on its own regardless), or a
    /// real "no changes" message for a genuinely clean worktree.
    pub(super) fn render_changes_rows(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(diff) = self.current_diff() else {
            return self.render_diff_state_message();
        };
        if diff.files.is_empty() {
            return render_sidebar_message("no changes".to_string(), theme::text::FAINT);
        }

        let rendered_count = diff.files.len().min(MAX_RENDERED_DIFF_FILES);
        let mut list = div().id("changes-rows").flex().flex_col();
        // `diff.truncated` is `wt_core::diff`'s own load-time cap firing (2MB of raw `git diff`
        // output, or more than 300 changed files - see `WorktreeDiff::truncated`'s docs) -
        // distinct from both a single file's own `DiffFile::truncated` (a per-file hunk-line
        // cap, surfaced separately in `Self::render_diff_file_detail`) and this list's own
        // `MAX_RENDERED_DIFF_FILES` *render* cap (the "... and N more changed files not shown"
        // message below, which only fires when there's real, fully-loaded data this view simply
        // chose not to render). Without this, a diff that hit `wt_core::diff`'s own cap looked
        // exactly like a complete one - a real regression from the pre-Phase-D Changes view.
        if diff.truncated {
            list = list.child(render_sidebar_message(
                "diff truncated: this worktree's real changes exceeded wt_core::diff's own \
                 load limits, so some files or lines are missing from this list"
                    .to_string(),
                theme::status::ASK,
            ));
        }
        for file in &diff.files[..rendered_count] {
            list = list.child(self.render_change_row(file, cx));
        }
        if diff.files.len() > rendered_count {
            list = list.child(render_sidebar_message(
                format!(
                    "... and {} more changed files not shown",
                    diff.files.len() - rendered_count
                ),
                theme::text::FAINT,
            ));
        }
        list.into_any_element()
    }

    /// One Changes row: a real review checkbox, `dir`/`name`, an optional tag pill, real
    /// `+n`/`−n`, and the real five-segment stat bar - `design_handoff_jerry_ade/README.md`'s
    /// Changes row spec. Clicking anywhere on the row (other than the checkbox itself - see
    /// [`Self::render_review_checkbox`]'s `stop_propagation`) opens this file's real diff in the
    /// centre via [`Self::open_change_diff`].
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

        div()
            .id(format!("change-row-{}", file.path.display()))
            .flex()
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
                        .text_size(px(10.5))
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
                    .text_size(px(11.5))
                    .text_color(if reviewed {
                        theme::text::DIMMER
                    } else {
                        theme::text::STRONG
                    })
                    .child(name),
            )
            .when_some(tag, |el, tag| el.child(render_tag_pill(tag)))
            // A rename-only file gets no `tag` from `changes::change_tag` (a plain rename isn't
            // `new`/`del`), so without this it rendered as a plain filename with `+0 -0` and an
            // empty stat bar - visually identical to a file with no changes at all. Real
            // signal, not decoration: `changes::is_real_rename` only fires when `old_path` is
            // both present and actually different from the current path.
            .when(changes::is_real_rename(file), |el| {
                el.child(render_moved_tag())
            })
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::diff::STAT_ADD)
                    .child(format!("+{add}")),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::diff::STAT_DEL)
                    .child(format!("\u{2212}{del}")),
            )
            .child(render_stat_bar(segments))
    }

    /// The Changes row's real 12×12 review checkbox (`design_handoff_jerry_ade/README.md`:
    /// "a 12×12 review checkbox (checked border `#2f6d4b`, bg `#24503a`, `✓` `#9fdcb6`)") - real
    /// interactive state via [`Self::toggle_reviewed`], not decoration. Stops propagation on
    /// click so checking a box never also opens the row's diff, mirroring
    /// `Self::render_session_tab`'s own nested-clickable-child pattern (its tab-close `×`).
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
            .text_size(px(9.0))
            .text_color(theme::button::GREEN_FG)
            .when(checked, |el| el.child("\u{2713}"))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.toggle_reviewed(path.clone(), cx);
            }))
    }
}

/// Which real data source the right sidebar currently shows for the selected worktree -
/// `design_handoff_jerry_ade/README.md`'s Zone 3 `right_pane` state (`Files | Changes`, `Files`
/// default). The panel itself never shows diff *content* (see [`AdeApp::open_change`]'s docs) -
/// `Changes` is the real per-file review list, not a diff view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RightSidebarView {
    Files,
    Changes,
}

/// The Changes list's real footer 29 (`design_handoff_jerry_ade/README.md`: "Footer 29: `click
/// a file to open its diff in the centre · ] next file`"). The `] next file` portion of that
/// spec text is deliberately dropped here: `]` isn't actually bound to anything (only
/// `secondary-n` is a real, wired-up keybinding - see `crate::default_key_bindings`), and advertising a
/// shortcut that silently does nothing if pressed is worse than a shorter, accurate footer.
pub(super) fn render_changes_footer() -> impl IntoElement {
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
        .text_size(px(10.0))
        .text_color(theme::text::HINT)
        .child("click a file to open its diff in the centre")
}

/// The file tree row's real `▾`/`▸` caret (`design_handoff_jerry_ade/Jerry.dc.html`'s tree row
/// template: an 8px-wide `n.caret` span, `#4a5057`, before the folder/file icon) - the signal
/// that a directory row is clickable/expandable, distinct from the folder icon itself. Blank
/// (but still 8px wide, to keep every row's icon column aligned) for a file row, which the
/// mockup's own data never gives a caret at all.
pub(super) fn render_tree_caret(is_dir: bool, open: bool) -> impl IntoElement {
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
        .text_size(px(9.0))
        .text_color(theme::text::TREE_CARET)
        .child(label)
}

/// The file tree's real folder icon - `design_handoff_jerry_ade/README.md`: "Folder icon is two
/// rects — a 5×3 tab at (0,1) and a 12×8 radius-2 body at (0,3) — outlined `#4e545a` when
/// collapsed, filled `#23272b` with a `#6b7178` border when open." Composed entirely from
/// nested `div()`s with real borders/backgrounds/rounded corners (per the design's own "Assets"
/// section: "Every icon is composed from rects and text glyphs ... precisely so that nothing
/// needs an SVG pipeline") - never an emoji glyph standing in for it, which is exactly what the
/// reported "tofu box" bug was: `\u{1F4C1}` folder/`\u{1F4C4}` file emoji with no matching glyph
/// installed on the machine that reported the bug.
///
/// The two rects are *not* styled identically, verified against `design_handoff_jerry_ade/
/// Jerry.dc.html`'s own real tree-row template (`n.folderBd`/`n.folderBg`, not this crate's own
/// paraphrase above): the 12×8 body alternates between a filled `bg` (open) and a transparent
/// one (collapsed), both with a real `border`, exactly as the doc comment above says - but the
/// 5×3 tab is always solid-filled with the state's `border` colour and has no separate border of
/// its own (`background:{{ n.folderBd }}` with nothing else). Rendering the tab with the same
/// hollow-when-collapsed treatment as the body (an earlier version of this function did) was a
/// real design-fidelity gap: the mockup's collapsed-folder tab is solid, not outlined.
pub(super) fn render_folder_icon(open: bool) -> impl IntoElement {
    let (fill, border) = if open {
        (theme::surface::CHIP_NEUTRAL, theme::text::FAINT)
    } else {
        (work_surface::TRANSPARENT, theme::text::GHOST)
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

/// The file tree's real 13×13 radius-2.5 language chip (`design_handoff_jerry_ade/README.md`'s
/// Zone 3 chip table) - a real rect with a real text-glyph label, per
/// `crate::file_tree::lang_chip_for_name`'s pure selection logic (never an emoji, never a
/// second, independent extension-matching guess at the tab-strip's own `rs`/`to`/`md`/`sq`
/// chips).
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

/// The Changes row's `moved` tag for a real rename with a different pre-rename path
/// (`changes::is_real_rename`) - a plain rename has no `ChangeTag` of its own
/// (`changes::change_tag` deliberately returns `None` for `Modified`/`Renamed` alike, since
/// most renames also carry a content change and already show real `+n`/`−n`), so a rename-only
/// file needs its own distinct visual signal instead of looking identical to "no changes at
/// all". Deliberately its own muted style, not [`ChangeTag`]'s bg/fg pair (that enum only
/// covers `new`/`del`, and reusing an unrelated colour for a third, semantically different
/// meaning was judged worse than a plain, honestly-neutral tag here).
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
