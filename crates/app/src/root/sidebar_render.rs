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
    /// the row itself re-scanning `diff.files` per row per frame - with up to 500 rendered
    /// rows against up to 300 diff files, that scan was a measured ~21ms foreground stall
    /// against a ~33ms frame budget. A deleted file never needs an entry here:
    /// `crate::file_tree::build_file_tree` only lists currently-existing entries.
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

    /// The file tree - `design_handoff_jerry_ade/README.md`'s Zone 3 "Files (tree)" spec:
    /// rect-composed folder/language-chip icons (see [`render_folder_icon`]/
    /// [`render_lang_chip`], never emoji or an SVG pipeline), collapse/expand (see
    /// [`Self::toggle_dir_collapsed`]/`crate::file_tree::visible_entries`) - and deliberately
    /// **no `size_full()`/fixed height on this list**. The caller
    /// (`Self::render_right_sidebar`) wraps it in `flex_1().min_h_0().overflow_y_scroll()`; a
    /// scrollable container's child must be free to grow to its natural content height, not
    /// clamped to the viewport, or content past the bottom is silently clipped instead of
    /// scrollable - the exact "file tree scroll doesn't work" bug this once was.
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

        // Only the first `file_tree::MAX_RENDERED_FILE_ENTRIES` *visible* rows become actual
        // elements - laying out more `div`s than that through GPUI's flexbox engine on every
        // render (as often as ~33ms while a terminal streams output) was a measured
        // foreground stall. A virtualized list (`uniform_list`, see
        // `vendor/zed/crates/gpui/src/elements/uniform_list.rs`) would scale further but is
        // out of scope here.
        let rendered_count = visible.len().min(file_tree::MAX_RENDERED_FILE_ENTRIES);

        // Built once per render, not once per row - see `Self::tree_change_marks`'s docs.
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

        let mut row = div()
            .id(format!("file-tree-row-{}", entry.path.display()))
            .flex()
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
                        .child(render_changes_footer(self.ui_text_size(10.0)))
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
            return self.render_diff_state_message();
        };
        if diff.files.is_empty() {
            return render_sidebar_message("no changes".to_string(), theme::text::FAINT);
        }

        let rendered_count = diff.files.len().min(MAX_RENDERED_DIFF_FILES);
        let mut list = div().id("changes-rows").flex().flex_col();
        // `diff.truncated` is `wt_core::diff`'s own load-time cap firing (2MB of raw `git diff`
        // output, or more than 300 changed files) - distinct from a single file's own
        // `DiffFile::truncated` (per-file hunk-line cap, surfaced in
        // `Self::render_diff_file_detail`) and this list's own `MAX_RENDERED_DIFF_FILES`
        // *render* cap below, which only ever omits already fully-loaded data.
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
