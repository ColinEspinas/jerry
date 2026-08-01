use super::*;
use crate::keymap;
use crate::root::widgets::{
    modal_scrim_bg, render_keycap_row, render_menu_group_divider, render_modal_button,
    render_sidebar_message, render_tag_pill, text_tooltip, KeycapSize,
};
use crate::settings::widgets::ChoiceOption;

impl AdeApp {
    /// Switches which data source the right sidebar shows. Switching *to* the Changes view
    /// always recomputes the diff (`load_diff`, not just `cx.notify()`) rather than showing
    /// whatever was last loaded - a stale snapshot from when the worktree was first selected
    /// would silently hide changes an agent just made.
    pub(crate) fn set_right_sidebar_view(
        &mut self,
        view: RightSidebarView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if view != RightSidebarView::Files {
            // Leaving the Files tab unrenders `Self::file_tree_shell` entirely - and with it the
            // node `AdeApp::tree_focus_handle` is `track_focus`'d on, plus every control the
            // tree's own overlays act through. Three real problems, all closed here (GitHub issue
            // #19, found in this change's own review):
            //
            // 1. **Dangling focus.** A right-click focuses the tree. Switching to Changes with
            //    focus still on `tree_focus_handle` leaves `Window::focus` pointing at a
            //    `FocusId` that `focus_node_id_in_rendered_frame` can no longer find, so GPUI
            //    falls back to the dispatch root - silently killing every context-scoped binding
            //    *and* the focused terminal's own key handling until the next click. This is the
            //    exact invariant `crate::root::OverlayFocus`' docs describe, applied to a tab
            //    switch rather than an overlay.
            // 2. A context menu targeting a row that is no longer on screen.
            // 3. A half-typed inline name for a row that is no longer on screen.
            //
            // The armed delete confirmation deliberately survives: it is a window-level modal
            // with its own scrim and buttons, not a tree affordance, and silently disarming a
            // destructive confirmation the user is mid-way through answering would be its own
            // small dishonesty.
            self.tree_context_menu = None;
            self.tree_inline_edit = None;
            if self.tree_focus_handle.is_focused(window) {
                restore_focus(&self.sessions, &mut self.code_focus, window, cx);
            }
            // 1 again, one level removed, and invisible to the check above: an *overlay* can be
            // holding the tree's handle as its own return target while the overlay itself has
            // focus - which is exactly the state the palette's own "Toggle Files / Changes"
            // command runs in, since the palette is focused and the tree is not. Closing the
            // palette afterwards would then restore focus onto a handle this very branch has
            // just unrendered. Swept from every overlay rather than only the palette, so a
            // future overlay reaching this path doesn't reintroduce it. Reproduced by this
            // change's own adversarial audit; see `OverlayFocus::forget_target`.
            self.palette_focus.forget_target(&self.tree_focus_handle);
            self.settings_focus.forget_target(&self.tree_focus_handle);
            self.code_focus.forget_target(&self.tree_focus_handle);
        }
        self.right_sidebar_view = view;
        if view == RightSidebarView::Changes {
            self.load_diff(self.diff_root.clone(), cx);
        } else {
            cx.notify();
        }
    }

    /// Toggles a directory's expanded state - `crate::sidebar::file_tree::visible_entries` does
    /// the actual hiding at render time, and [`Self::set_dir_expanded`] records the change in
    /// memory and queues an immediate background write of it (never a write awaited here on the
    /// foreground thread).
    pub(crate) fn toggle_dir_expanded(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let expanded = !self.expanded_dirs.contains(&path);
        self.set_dir_expanded(path, expanded, cx);
    }

    /// Records one directory's expanded state in the live set and in the persisted
    /// [`AdeApp::fold_state`] - **the** single place either is mutated for a single directory, so
    /// [`Self::set_dir_expanded`] and [`Self::reveal_in_tree`] can't drift apart (they used to be
    /// two hand-copied bodies, log string and all). Returns whether the persisted state changed,
    /// leaving the caller to decide when to write and when to notify: a reveal touches a whole
    /// ancestor chain and wants one write for all of it, not one per level.
    ///
    /// The tree still expands when the state is *refusable* (a non-UTF-8 path, which has no
    /// honest TOML key - see `fold_state::worktree_key`): silently declining to open a folder
    /// because of how its name is encoded would be a far worse outcome than not remembering that
    /// it was opened. But it says so in the log rather than leaving the live tree and the file
    /// quietly disagreeing.
    ///
    /// Uses the cached [`AdeApp::fold_state_root_key`], never `fold_state::worktree_key` - see
    /// that field's docs for the blocking-syscall-per-click this avoids.
    pub(in crate::sidebar) fn record_dir_expanded(&mut self, path: &Path, expanded: bool) -> bool {
        if expanded {
            self.expanded_dirs.insert(path.to_path_buf());
        } else {
            self.expanded_dirs.remove(path);
        }
        let root = self.file_tree_root.clone();
        let outcome = match self.fold_state_root_key.clone() {
            Some(root_key) => {
                let outcome = self
                    .fold_state
                    .set_expanded_with_key(&root_key, &root, path, expanded);
                if outcome == fold_state::SetExpanded::Changed {
                    // Claimed here rather than at each call site: "I changed this worktree's
                    // entry" and "this worktree's entry is mine to overwrite on the next merged
                    // write" are the same fact, and splitting them is how one of them gets
                    // forgotten.
                    self.fold_state_owned.insert(root_key);
                }
                outcome
            }
            None => fold_state::SetExpanded::Refused,
        };
        if outcome == fold_state::SetExpanded::Refused {
            log::warn!(
                "not recording the fold state of {} under {}: it is not a plain UTF-8 path \
                 inside that worktree, so this expansion won't survive a restart",
                path.display(),
                root.display()
            );
        }
        outcome == fold_state::SetExpanded::Changed
    }

    /// One directory's expand/collapse, recorded and queued for an immediate background write
    /// (GitHub issue #18 §2 - "expanding or collapsing a folder is recorded immediately, not only
    /// on clean exit"). The write is queued, not awaited: see [`AdeApp::persist_fold_state`] for
    /// the serial writer loop it hands off to, and what happens when that write fails.
    pub(crate) fn set_dir_expanded(
        &mut self,
        path: PathBuf,
        expanded: bool,
        cx: &mut Context<Self>,
    ) {
        if self.record_dir_expanded(&path, expanded) {
            self.persist_fold_state(cx);
        }
        cx.notify();
    }

    /// "Collapse all" - resets the tree *and* the saved state for this worktree in one step
    /// (issue #18 §1), so relaunching doesn't bring the old expansions back.
    pub(crate) fn collapse_all_dirs(&mut self, cx: &mut Context<Self>) {
        self.expanded_dirs.clear();
        if let Some(root_key) = self.fold_state_root_key.clone() {
            if self.fold_state.clear_worktree_with_key(&root_key) {
                // Claimed *because* the entry is now gone: the merge on write treats an owned
                // key's absence as a real deletion, which is exactly what this action means.
                self.fold_state_owned.insert(root_key);
                self.persist_fold_state(cx);
            }
        }
        cx.notify();
    }

    /// Reveals `path` in the tree by expanding every ancestor directory between it and the tree
    /// root - and records each of those expansions exactly like a manual click would (issue #18
    /// §5). The one entry point for every "reveal in tree" flow: a palette file result
    /// (`Self::open_palette_file_result`) and a just-created file (`Self::create_new_file`),
    /// both of which would otherwise point at a row hidden inside a collapsed parent now that
    /// the tree starts collapsed.
    ///
    /// `path` itself is deliberately not expanded when it happens to be a directory: revealing
    /// something means making it visible, not opening it.
    pub(crate) fn reveal_in_tree(&mut self, path: &Path, cx: &mut Context<Self>) {
        let root = self.file_tree_root.clone();
        let mut changed = false;
        for ancestor in path.ancestors().skip(1) {
            if ancestor == root {
                break;
            }
            if !ancestor.starts_with(&root) {
                // Not part of this tree at all - nothing to reveal, and certainly nothing to
                // record under this worktree's key.
                break;
            }
            // The same real recording path a manual click takes, refusal logging included -
            // one shared helper, not a second copy of it.
            changed |= self.record_dir_expanded(ancestor, true);
        }
        if changed {
            self.persist_fold_state(cx);
        }
        // A reveal genuinely changes what the tree shows, so it must repaint on its own rather
        // than relying on every caller happening to notify afterwards.
        cx.notify();
    }

    /// Drops fold-state entries for directories that no longer exist, against the tree that has
    /// just finished loading (issue #18 §2 - "stale entries are silently ignored and pruned,
    /// never an error"). Called from `Self::load_file_tree`'s completion handler, which has
    /// already checked that the walk it is applying belongs to the current root.
    ///
    /// Only ever prunes against a walk that is genuinely *complete*
    /// ([`file_tree::FileTreeListing::is_complete`]). A walk that stopped at its entry cap - or
    /// that silently skipped a directory it couldn't read, which the walk does deliberately so
    /// one unreadable folder can't blank the whole sidebar - is not evidence that the
    /// directories it never reached are gone. Pruning against either would permanently delete
    /// perfectly good state, since the prune is written straight back to disk.
    pub(crate) fn prune_stale_fold_state(&mut self, cx: &mut Context<Self>) {
        if !self.file_tree_complete {
            return;
        }
        let dirs = file_tree::directory_paths(&self.file_tree);
        let root = self.file_tree_root.clone();
        let Some(root_key) = self.fold_state_root_key.clone() else {
            return;
        };
        let mut changed = self
            .fold_state
            .prune_missing_dirs_with_key(&root_key, &root, &dirs);
        let before = self.expanded_dirs.len();
        self.expanded_dirs.retain(|path| dirs.contains(path));
        changed |= self.expanded_dirs.len() != before;
        if changed {
            self.fold_state_owned.insert(root_key);
            self.persist_fold_state(cx);
        }
    }

    /// Whether the current walk budget is already at [`file_tree::MAX_LOAD_MORE_ENTRIES`], so
    /// [`Self::load_more_file_tree_entries`] has nothing left to raise - the condition that turns
    /// the sidebar's action row into a plain disclosure rather than a button that re-walks a
    /// ceiling's worth of entries to no effect.
    pub(crate) fn file_tree_limit_is_at_ceiling(&self) -> bool {
        self.file_tree_limit_override
            .unwrap_or(self.settings.file_tree.max_entries)
            >= file_tree::MAX_LOAD_MORE_ENTRIES
    }

    /// The "load more" action's real handler - re-walks the current tree with a tenfold larger
    /// entry budget. Genuinely reloads rather than revealing something already loaded: the
    /// capped walk never collected those entries. Still bounded, deliberately - see
    /// [`AdeApp::file_tree_limit_override`] for why this raises the cap instead of removing it.
    pub(crate) fn load_more_file_tree_entries(&mut self, cx: &mut Context<Self>) {
        let current = self
            .file_tree_limit_override
            .unwrap_or(self.settings.file_tree.max_entries);
        // Ceilinged *and* monotonic. The ceiling is why this can't escalate into the unbounded
        // walk again (see `file_tree::MAX_LOAD_MORE_ENTRIES`); the `.max(current)` is a real bug
        // fix rather than defensive padding - a `max_entries` above the ceiling would otherwise
        // make one click *shrink* the budget, so "load more" would visibly remove rows from the
        // tree. A limit this action produces can never be smaller than the one it replaced.
        let next = current
            .saturating_mul(10)
            .min(file_tree::MAX_LOAD_MORE_ENTRIES)
            .max(current);
        if next == current {
            // Already at the ceiling: re-walking would burn a whole budget's worth of work to
            // produce byte-identical rows. `render_file_tree` doesn't offer the action at all in
            // this state (it renders a plain disclosure instead); this is the same fact enforced
            // at the handler, so no other caller can reintroduce the dead-but-expensive click.
            return;
        }
        self.file_tree_limit_override = Some(next);
        self.load_file_tree(self.file_tree_root.clone(), cx);
    }

    /// Toggles a file's reviewed state - the Changes row checkbox's click handler.
    /// `Self::render_change_row` stops propagation at the call site so checking a box never
    /// also opens that file's diff.
    pub(in crate::sidebar) fn toggle_reviewed(&mut self, path: PathBuf, cx: &mut Context<Self>) {
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
    /// `crate::sidebar::file_tree::build_file_tree` only lists currently-existing entries.
    pub(in crate::sidebar) fn tree_change_marks(
        &self,
    ) -> HashMap<PathBuf, (&'static str, gpui::Rgba)> {
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
    /// [`Self::toggle_dir_expanded`]/`crate::sidebar::file_tree::visible_entries`).
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
    pub(in crate::sidebar) fn render_file_tree(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Both early returns keep their own scroll box. `Self::render_right_sidebar`'s Files arm
        // can no longer be a scroller itself (the `uniform_list` below owns scrolling), but
        // these two paths render no list at all - and a real `std::io::Error` from an
        // unreadable directory is an arbitrarily long string that used to be scrollable inside
        // that outer container. Dropping it would silently clip the very message the user needs
        // in order to understand why the tree is empty.
        //
        // Both are still wrapped by [`Self::file_tree_shell`], so an unreadable or empty
        // directory still has a focusable tree with a working empty-area context menu - "New
        // file" on a directory with nothing in it yet is exactly when that menu is most needed
        // (GitHub issue #19 §1).
        if let Some(error) = &self.file_tree_error {
            let message = scrollable_sidebar_message(
                "file-tree-error",
                format!("failed to read directory: {error}"),
                theme::status::FAIL.into(),
            );
            // An in-progress name editor is still drawn here, above the error. A walk can start
            // failing *while* a name is being typed (an agent removing the folder underneath it),
            // and the alternative - showing only the error - would leave the editor's typed text
            // alive in `AdeApp::tree_inline_edit` and its `"tree-editing"` key context alive on
            // this node, with nothing on screen to explain why every tree keybinding had gone
            // dead. Discarding the text instead would break issue #19 §4's own requirement.
            let body = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .w_full()
                .when(self.tree_inline_edit.is_some(), |el| {
                    el.child(self.render_tree_inline_edit_row(0, cx))
                })
                .child(message)
                .into_any_element();
            return self.file_tree_shell(body, cx);
        }
        if self.file_tree.is_empty() && self.tree_inline_edit.is_none() {
            let message = scrollable_sidebar_message(
                "file-tree-empty",
                "(empty directory)".to_string(),
                theme::text::FAINT.into(),
            );
            return self.file_tree_shell(message, cx);
        }

        // Every visible row is rendered - there is no render-time cap and no "... and N more
        // entries not shown" row any more (GitHub issue #18 §4). The list below is virtualized,
        // so this count drives `uniform_list`'s scroll extent, not how many elements get built.
        //
        // Resolved *once* per frame, into indices into `self.file_tree`, and shared with the
        // row-builder closure through an `Rc`. That indirection is load-bearing: `uniform_list`
        // invokes its closure **three** times per frame (`measure_item` from `request_layout`,
        // again from `prepaint`, then `render_items` for the real visible range -
        // `vendor/zed/crates/gpui/src/elements/uniform_list.rs:283`, `:359`, `:489`), so a
        // `visible_entries` call inside it would walk the whole loaded tree four times per frame
        // including this one. Indices rather than the borrowed `&FileTreeEntry`s the walk
        // returns, because the closure is `'static` and cannot hold a borrow of `self` - and
        // indices are valid for exactly as long as they need to be, since nothing can mutate
        // `file_tree` between this line and the closure's last call within one frame. They are
        // still bounds-checked below rather than indexed blindly.
        let visible_indices = file_tree::visible_indices(&self.file_tree, &self.expanded_dirs);
        // The in-progress inline name editor is woven into that same row list as a real row
        // (issue #19 §2: "an inline name editor at the right spot in the tree"), rather than
        // floating over it - so it scrolls with the tree, is indented like its neighbours, and
        // costs the virtualization nothing.
        //
        // Its position is re-derived here, by *path*, on every frame. That is what makes it
        // survive a walk that replaced every row underneath it (issue #19 §4): the editor's own
        // state lives on `AdeApp`, not in `file_tree`, and this lookup simply re-finds its
        // anchor - falling back to the top of the list when the anchor genuinely no longer
        // exists, rather than dropping the editor and the user's typed text with it.
        let (rows, editor_depth) = self.file_tree_rows(&visible_indices);
        let rows: Rc<Vec<TreeRow>> = Rc::new(rows);
        let rendered_count = rows.len();

        // Built once per render, not once per row - see `Self::tree_change_marks`'s docs, and
        // `visible_indices` above for why anything the row-builder closure needs is captured
        // rather than recomputed inside it.
        let marks = self.tree_change_marks();

        // Virtualized: only the rows genuinely on screen become real elements. Previously every
        // one of up to 500 (the since-removed `MAX_RENDERED_FILE_ENTRIES` cap) *visible* rows was built,
        // laid out and painted on every single frame - including the ~460 of them scrolled off
        // screen - which measured (real `gpui::FrameTiming` data, debug build, this repository's
        // own tree, terminal streaming) as ~145ms of a ~200ms `Window::draw`, i.e. ~72% of the
        // entire frame, holding the whole app at ~4fps. `uniform_list` is the same real
        // virtualization the File view's own code list already uses
        // (`crate::code_surface::file_view::AdeApp::render_file_view`,
        // `vendor/zed/crates/gpui/examples/uniform_list.rs`); every row here is exactly
        // `theme::band::TREE_ROW` tall, which is `uniform_list`'s one real requirement.
        //
        // The former `MAX_RENDERED_FILE_ENTRIES` (500) cap is gone: virtualization already
        // removed the per-frame cost it was originally guarding, and hiding real entries behind
        // a "... and N more" row was the dishonesty issue #18 §4 set out to remove. The one
        // surviving cap is a *load*-time one (`Settings.file_tree.max_entries`), and it announces
        // itself with the real "load more" action below rather than a silent cut-off.
        let list = uniform_list(
            "file-tree-list",
            rendered_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                // Both the row range and each index into `file_tree` are clamped/checked rather
                // than trusted, so a future divergence degrades to "renders fewer rows" instead
                // of panicking. `start` is clamped to `end`, not just to the length: clamping
                // only the upper bound still leaves `start > end`, which panics in the slice
                // expression below rather than degrading.
                let end = range.end.min(rows.len());
                let start = range.start.min(end);
                rows[start..end]
                    .iter()
                    .filter_map(|row| match row {
                        TreeRow::Entry(index) => this
                            .file_tree
                            .get(*index)
                            .cloned()
                            .map(|entry| this.render_file_tree_row(&entry, &marks, cx)),
                        TreeRow::InlineEditor => {
                            Some(this.render_tree_inline_edit_row(editor_depth, cx))
                        }
                    })
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
        .min_h_0()
        // GitHub issue #30's real overlay scrollbar reads its geometry straight off this same
        // handle (`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`) - not a second,
        // parallel tracking mechanism.
        .track_scroll(&self.file_tree_scroll_handle);

        // The scrollbar is a *sibling* of `list`, inside its own non-scrolling `.relative()`
        // wrapper - never a child of `list` itself. `Interactivity::prepaint`'s own real scroll
        // translation (`vendor/zed/crates/gpui/src/elements/div.rs:1844-1851`'s
        // `window.with_element_offset(scroll_offset, ...)`) applies uniformly to *every* child of
        // a scrolling element, including an absolutely-positioned one - verified directly, not
        // assumed - so a scrollbar painted as `list`'s own child would scroll away with the rows
        // instead of staying pinned like a real overlay. The wrapper keeps `list`'s own
        // `flex_1().min_h_0()` working exactly as before (a `.relative()` div is still a normal
        // flex parent once `.flex().flex_col()` is added) by taking over the *outer* flex slot
        // `list` used to occupy directly inside `column`, below.
        let list = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .children(self.render_vertical_scrollbar(
                "file-tree-scrollbar",
                &self.file_tree_scroll_handle,
                &[],
                cx,
            ));

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
        // The explicit replacement for the old truncation row: not a message saying entries were
        // silently dropped, but a real action that goes and loads more of them
        // (`Self::load_more_file_tree_entries`). Only ever shown when the walk genuinely stopped
        // early, and it names the exact count it stopped at rather than implying a total it
        // cannot know without walking the rest.
        if self.file_tree_truncated && self.file_tree_limit_is_at_ceiling() {
            // The honest end of the escalation: still truncated, but no larger budget is
            // available, so an action here would be a button that re-walks a whole ceiling's
            // worth of entries to produce exactly the same rows. A plain, non-interactive
            // disclosure instead - it still names the count, so the listing is never *silently*
            // cut off, which is the actual requirement.
            column = column.child(render_sidebar_message(
                format!(
                    "Showing the first {} entries (the most this tree can load)",
                    self.file_tree.len()
                ),
                theme::text::FAINT.into(),
            ));
        } else if self.file_tree_truncated {
            let loaded = self.file_tree.len();
            column = column.child(
                div()
                    .id("file-tree-show-all")
                    .debug_selector(|| "file-tree-show-all".to_string())
                    .flex_none()
                    .w_full()
                    .cursor_pointer()
                    .px(px(10.0))
                    .py(px(5.0))
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::DIM)
                    .hover(|el| {
                        el.bg(theme::surface::ROW_HOVER)
                            .text_color(theme::text::PRIMARY)
                    })
                    .tooltip(text_tooltip(
                        "Re-read this worktree with a 10x larger entry limit. Set \
                         `file_tree.max_entries` in settings.toml to raise it permanently.",
                    ))
                    .child(format!("Stopped at {loaded} entries \u{2013} load more"))
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.load_more_file_tree_entries(cx);
                    })),
            );
        }

        // The real, honest surface for a failed file operation (a refused rename, a trash
        // command that didn't run) - next to the tree it happened in, not buried in the log.
        if let Some(error) = self.tree_op_error.clone() {
            column = column.child(
                div()
                    .id("file-tree-op-error")
                    .debug_selector(|| "file-tree-op-error".to_string())
                    .flex_none()
                    .w_full()
                    .px(px(10.0))
                    .py(px(5.0))
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::status::FAIL)
                    .cursor_pointer()
                    .tooltip(text_tooltip("Click to dismiss"))
                    .child(error)
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.tree_op_error = None;
                        cx.notify();
                    })),
            );
        }

        self.file_tree_shell(column.into_any_element(), cx)
    }

    /// The tree's real *outer* element - the one node that carries keyboard focus, the
    /// `"file-tree"` key context every tree keybinding is scoped to, the empty-area right-click,
    /// and the `gpui::canvas` that records where the tree is painted (GitHub issue #19 §1).
    ///
    /// Wrapping every arm of [`Self::render_file_tree`] - including the "(empty directory)" and
    /// unreadable-directory messages - rather than only the list arm is deliberate: an empty
    /// directory is precisely when "right-click → New file" matters most, and a tree that lost
    /// its focus target whenever a walk failed would silently disable every one of its
    /// keybindings until the next successful walk.
    ///
    /// While an inline name editor is open the context string gains *two* more words,
    /// `"tree-editing"` and `"text-input"`. The first is the real mechanism that stops
    /// `Ctrl+C`/`Ctrl+X`/`Ctrl+V`/`F2`/`Shift+F10` from firing while the user is typing a name -
    /// see `crate::sidebar::tree_ops`'s module docs and `crate::default_key_bindings`' own
    /// entries for why an extra context word is used rather than conditionally omitting the
    /// bindings. The second is what routes `Ctrl+Z` mid-filename to `TextUndo`.
    ///
    /// The literals themselves come from `crate::keymap_overrides::file_tree_key_context`, which
    /// `real_context_stacks()` also calls, so the renderer and the enumeration every scoping
    /// claim rests on cannot drift apart. That function's docs carry the reasoning.
    fn file_tree_shell(&self, body: gpui::AnyElement, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Space-separated context *words*, which is what `KeyBindingContextPredicate`'s
        // identifier terms match against - so `Some("file-tree && !tree-editing")` really does
        // stop matching the moment `"tree-editing"` is added. Two independent modal states add a
        // word each (an open inline name editor, and the modal delete confirmation, which would
        // otherwise let `F2`/`Shift+F10` fire behind its own scrim), and an open editor adds
        // `"text-input"` on top - GitHub issue #17's one shared tag for every real text-typing
        // surface, routing `Ctrl+Z` in a rename box to `TextUndo` - see
        // `crate::sidebar::tree_ops::AdeApp::handle_tree_text_undo`, the listener the tag routes
        // to.
        let key_context = crate::keymap_overrides::file_tree_key_context(
            self.tree_inline_edit.is_some(),
            self.tree_delete_confirm.is_some(),
        );
        div()
            .id("file-tree-shell")
            .key_context(key_context)
            .track_focus(&self.tree_focus_handle)
            .on_action(cx.listener(Self::handle_file_tree_context_menu_action))
            .on_action(cx.listener(Self::handle_file_tree_rename_action))
            .on_action(cx.listener(Self::handle_file_tree_copy_action))
            .on_action(cx.listener(Self::handle_file_tree_cut_action))
            .on_action(cx.listener(Self::handle_file_tree_paste_action))
            .on_action(cx.listener(Self::handle_tree_text_undo))
            .on_action(cx.listener(Self::handle_tree_text_redo))
            .on_key_down(cx.listener(Self::handle_tree_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            // The empty area below the last row: this container's own right-click, which a row's
            // handler pre-empts with `cx.stop_propagation()` (GPUI dispatches a mouse listener
            // on the deepest element first and stops walking outwards once propagation is
            // stopped - `vendor/zed/crates/gpui/src/window.rs`'s `dispatch_mouse_event`).
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_tree_context_menu(
                        context_menu::ContextTarget::Empty,
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        window,
                        cx,
                    );
                }),
            )
            .child({
                // Where a keyboard-opened menu (`Shift+F10`) anchors - the same real
                // `gpui::canvas` bounds-capture pattern `Self::render_tab_strip_plus` uses for
                // the `+` button's own popover.
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.file_tree_bounds = bounds;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .child(body)
            .into_any_element()
    }

    /// The rows [`Self::render_file_tree`]'s `uniform_list` will build, and the indent depth the
    /// inline editor row (if any) should be drawn at.
    ///
    /// A rename *replaces* its target's row - the editor is that row, for as long as it's open.
    /// A New File / New Folder editor is *inserted* immediately after its parent folder's row, at
    /// one level deeper, which is where the created entry itself will appear. An editor whose
    /// anchor isn't in the visible list (a new entry in the worktree root, which has no row of
    /// its own; or an anchor a fresh walk no longer lists) goes to the top of the list rather
    /// than being dropped - see [`Self::render_file_tree`]'s own docs.
    fn file_tree_rows(&self, visible_indices: &[usize]) -> (Vec<TreeRow>, usize) {
        let mut rows: Vec<TreeRow> = visible_indices
            .iter()
            .copied()
            .map(TreeRow::Entry)
            .collect();
        let Some(edit) = self.tree_inline_edit.as_ref() else {
            return (rows, 0);
        };
        let anchor = edit.kind.anchor();
        // `.get()`, not direct indexing, per this file's own stated discipline for `file_tree`
        // indices (see `render_file_tree`'s comment on `visible_indices`): a future divergence
        // degrades to "the editor goes to the top of the list" rather than panicking.
        let anchor_row = visible_indices.iter().position(|index| {
            self.file_tree
                .get(*index)
                .is_some_and(|entry| entry.path == anchor)
        });
        let anchor_depth = anchor_row
            .and_then(|row| visible_indices.get(row))
            .and_then(|index| self.file_tree.get(*index))
            .map(|entry| entry.depth);
        match (&edit.kind, anchor_row) {
            (tree_ops::InlineEditKind::Rename { .. }, Some(row)) => {
                rows[row] = TreeRow::InlineEditor;
                (rows, anchor_depth.unwrap_or(0))
            }
            (tree_ops::InlineEditKind::Rename { .. }, None) => {
                rows.insert(0, TreeRow::InlineEditor);
                (rows, 0)
            }
            (_, Some(row)) => {
                rows.insert(row + 1, TreeRow::InlineEditor);
                (rows, anchor_depth.unwrap_or(0) + 1)
            }
            (_, None) => {
                rows.insert(0, TreeRow::InlineEditor);
                (rows, 0)
            }
        }
    }

    /// The inline name editor's row (issue #19 §2) - a real, append/backspace-only text field
    /// drawn at `depth`'s indentation, with the typed name, a caret, and the real rejection hint
    /// when one applies.
    ///
    /// It has no focus handle of its own: keystrokes reach it through
    /// [`Self::file_tree_shell`]'s `on_key_down`, which the tree's own focus already delivers.
    /// One focus target for the tree and its editor is what makes the
    /// `"file-tree tree-editing"` context switch above a single, honest fact rather than two
    /// handles that could disagree about which is focused.
    fn render_tree_inline_edit_row(
        &self,
        depth: usize,
        _cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(edit) = self.tree_inline_edit.as_ref() else {
            return div().into_any_element();
        };
        let indent = px(file_tree::INDENT_STEP * depth as f32);
        let name = edit.name.as_str().to_string();
        // Exactly `theme::band::TREE_ROW` tall, like every other row - a `uniform_list`'s one
        // real requirement is that every row is the same height, so the rejection hint is a
        // trailing element *inside* this row rather than a second line under it (which would
        // silently overlap the row below).
        div()
            .debug_selector(|| "file-tree-inline-edit".to_string())
            .flex()
            .items_center()
            .gap(px(6.0))
            .w_full()
            .h(theme::band::TREE_ROW)
            .pl(px(file_tree::ROW_LEFT_PAD) + indent)
            .pr(px(8.0))
            .bg(theme::surface::ROW_SELECTED)
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(11.5))
            .child(
                div()
                    .flex_none()
                    .text_size(self.ui_text_size(9.0))
                    .text_color(theme::text::GHOST)
                    .child(edit.kind.title()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(theme::text::STRONG)
                    // A real caret glyph, so an empty field still reads as "type here" rather
                    // than as a blank row - blinking (GitHub issue #27), same shared loop
                    // (`Self::tree_focus_handle` is wired into `crate::root::caret_blink` too) the
                    // code editor's/palette's own carets use. Same reasoning as the palette's own
                    // caret for why no separate "unfocused" case is needed here: this row only
                    // renders while `Self::tree_inline_edit` is genuinely open and
                    // `tree_focus_handle` is its own real focus target for that whole time.
                    .child(if self.caret_blink_visible {
                        format!("{name}\u{2502}")
                    } else {
                        name
                    }),
            )
            .when_some(edit.error.clone(), |el, error| {
                el.child(
                    div()
                        .debug_selector(|| "file-tree-inline-edit-error".to_string())
                        .flex_none()
                        .overflow_hidden()
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::status::FAIL)
                        .child(error),
                )
            })
            .into_any_element()
    }

    /// One file-tree row: indent (13px/level, per the README), a composed icon (a folder's
    /// two-rect glyph or a file's language chip), the name, and, for a directory, a click
    /// handler that toggles its membership of [`AdeApp::expanded_dirs`].
    pub(in crate::sidebar) fn render_file_tree_row(
        &self,
        entry: &FileTreeEntry,
        marks: &HashMap<PathBuf, (&'static str, gpui::Rgba)>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let indent = px(file_tree::INDENT_STEP * entry.depth as f32);
        let is_open = entry.is_dir && self.expanded_dirs.contains(&entry.path);
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
            // Positioning context for this row's own indent guides, below - `.absolute()` needs
            // a `.relative()` ancestor (`vendor/zed/crates/gpui/examples/list_example.rs:118`'s
            // own scrollbar track/thumb pair uses exactly this pairing).
            .relative()
            .w_full()
            .items_center()
            .gap(px(6.0))
            .h(theme::band::TREE_ROW)
            .pl(px(file_tree::ROW_LEFT_PAD) + indent)
            .pr(px(8.0))
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(11.5))
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED));

        // Indent guides (issue #18 §3), one per level of nesting between this row and the root.
        //
        // Drawn as this row's *own* absolutely-positioned children rather than as one overlay
        // across the list, which is what makes them correct under `uniform_list`'s
        // virtualization for free: a guide is a pure function of the row it belongs to
        // (`entry.depth`, plus the selected path for the active-chain highlight), so a recycled
        // row can only ever draw the guides that genuinely belong to whatever row it now shows.
        // An overlay would instead have to track the visible range and scroll offset itself and
        // stay in step with them - the real source of the "gaps or misaligned segments as rows
        // recycle" failure the issue calls out. Each guide spans the row's full 22px height with
        // no gap or inset, so consecutive rows' segments meet exactly and read as one continuous
        // line down the subtree.
        let active_levels = file_tree::active_guide_levels(
            &self.file_tree_root,
            &entry.path,
            self.selected_tree_path.as_deref(),
        );
        for level in 0..entry.depth {
            let active = level < active_levels;
            row = row.child(
                div()
                    // Test-only (a no-op outside test builds, like the row's own selector
                    // above): the only way a real render test can prove a guide painted at the
                    // right x, at the right height, on the right row - including after
                    // `uniform_list` has recycled that row's element. The `active`/`idle` half
                    // is what makes the ancestor-chain highlight testable at all: the colour
                    // itself isn't observable from a test, but which of the two branches a given
                    // row took is. Keyed on `entry.name` like the row selector above, so two
                    // same-named files in different folders would collide - every test using
                    // these gives its fixtures unique names.
                    .debug_selector(|| {
                        format!(
                            "file-tree-guide-{}-{}-{level}",
                            if active { "active" } else { "idle" },
                            entry.name
                        )
                    })
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(1.0))
                    .left(px(file_tree::indent_guide_x(level)))
                    .bg(if active {
                        theme::tree::INDENT_GUIDE_ACTIVE
                    } else {
                        theme::tree::INDENT_GUIDE
                    }),
            );
        }

        // This row's own right-click (GitHub issue #19 §1). `cx.stop_propagation()` is what keeps
        // it from *also* reaching `Self::file_tree_shell`'s empty-area handler, which would
        // otherwise replace this row's menu with the empty-area one a moment later.
        {
            let path = entry.path.clone();
            let is_dir = entry.is_dir;
            row = row.on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let target = if is_dir {
                        context_menu::ContextTarget::Folder(path.clone())
                    } else {
                        context_menu::ContextTarget::File(path.clone())
                    };
                    this.open_tree_context_menu(
                        target,
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        window,
                        cx,
                    );
                }),
            );
        }

        if entry.is_dir {
            let path = entry.path.clone();
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    // Selecting *and* focusing, both real: a folder click is what gives the tree
                    // keyboard focus (so its `Ctrl+C`/`F2`/`Shift+F10` bindings can match at
                    // all) and what gives those bindings a target. Deliberately here, in the
                    // click handler, and not inside `toggle_dir_expanded` - that method is also
                    // called programmatically (`start_tree_new_entry`, the reveal paths), where
                    // moving the selection would be a side effect nobody asked for.
                    this.selected_tree_path = Some(path.clone());
                    this.focus_file_tree(window, cx);
                    this.toggle_dir_expanded(path.clone(), cx);
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
    /// (`crate::sidebar::changes::diff_file_stats`) the Changes rows themselves show.
    pub(in crate::sidebar) fn render_right_sidebar_toggle(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = match self.right_sidebar_view {
            RightSidebarView::Files => "Files",
            RightSidebarView::Changes => "Changes",
        };
        let toggle = self.render_choice_control(
            "right-sidebar-toggle",
            &[ChoiceOption::new("Files"), ChoiceOption::new("Changes")],
            selected.to_string(),
            cx,
            |this, index, window, cx| {
                // Structural, not a label re-match: index 0 is `Files`, index 1 is `Changes`,
                // per the `options` array literal right above - see
                // `Self::render_choice_control`'s own docs for why dispatch is index-based.
                let view = match index {
                    1 => RightSidebarView::Changes,
                    _ => RightSidebarView::Files,
                };
                this.set_right_sidebar_view(view, window, cx);
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
            // "Collapse all" (issue #18 §1) - resets the tree *and* this worktree's saved fold
            // state in one step, so it genuinely undoes the expansions rather than hiding them
            // until the next launch. Files view only, like the "+" beside it.
            .when(self.right_sidebar_view == RightSidebarView::Files, |el| {
                el.child(
                    div()
                        .id("file-tree-collapse-all")
                        .debug_selector(|| "file-tree-collapse-all".to_string())
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
                        .tooltip(text_tooltip(
                            "Collapse all folders (also clears the saved fold state)",
                        ))
                        // The same "\u{25be} pointing down" glyph `render_tree_caret` uses for an
                        // open folder, since this is the action that closes every one of them.
                        .child("\u{25be}")
                        .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.collapse_all_dirs(cx);
                        })),
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
    pub(crate) fn render_right_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            RightSidebarView::Files => container
                .child(
                    div()
                        .id("right-sidebar-body")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_file_tree(cx)),
                )
                .child(render_file_tree_footer(
                    self.ui_text_size(10.0),
                    self.window_controls_style().is_macos(),
                    self.tree_inline_edit.is_none() && self.tree_delete_confirm.is_none(),
                )),
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
    pub(in crate::sidebar) fn render_changes_header(
        &self,
        diff: &WorktreeDiff,
    ) -> impl IntoElement {
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
    pub(in crate::sidebar) fn render_changes_rows(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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
        let changes_list = uniform_list(
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
        .min_h_0()
        .track_scroll(&self.changes_rows_scroll_handle);

        // See `Self::render_file_tree`'s own docs on why the scrollbar must be a sibling of the
        // list, inside its own non-scrolling `.relative()` wrapper, never a child of `list`
        // itself.
        column = column.child(
            div()
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .child(changes_list)
                .children(self.render_vertical_scrollbar(
                    "changes-rows-scrollbar",
                    &self.changes_rows_scroll_handle,
                    &[],
                    cx,
                )),
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
    /// [`Self::open_change_diff`] - which, since GitHub issue #15, also reveals and highlights
    /// that file in the Files tree, the same as every other way of opening a file in this app.
    pub(in crate::sidebar) fn render_change_row(
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
    pub(in crate::sidebar) fn render_review_checkbox(
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
pub(crate) enum RightSidebarView {
    Files,
    Changes,
}

/// One row of [`AdeApp::render_file_tree`]'s virtualized list: either a real walked entry (by
/// index into [`AdeApp::file_tree`]) or the in-progress inline name editor.
///
/// An index rather than the entry itself, for the same reason
/// `crate::sidebar::file_tree::visible_indices` returns indices: the `uniform_list` row-builder
/// closure is `'static` and cannot hold a borrow of `self`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeRow {
    Entry(usize),
    InlineEditor,
}

impl AdeApp {
    /// The file tree's right-click context menu popover (GitHub issue #19 §1).
    ///
    /// The same real overlay shape `crate::work_surface::render::AdeApp::render_plus_menu`
    /// established for this app's other floating popover - a full-window transparent scrim whose
    /// `on_click` dismisses ("click-away dismisses"), plus an absolutely-positioned panel that
    /// stops that click from bubbling. Zed's own `ui::ContextMenu` is not reachable here: it
    /// lives in Zed's `ui` crate, which this workspace deliberately doesn't depend on (only
    /// `gpui`/`gpui_platform`).
    ///
    /// The panel's origin is [`AdeApp::tree_context_menu`]'s already-clamped one, resolved at
    /// open time from the real click and the real `Window::bounds()` - so the menu near a window
    /// edge is repositioned once, not re-solved (and possibly moved) on every frame it's open.
    ///
    /// ## The scrim genuinely blocks what is behind it
    ///
    /// `.occlude()` (`gpui::InteractiveElement::occlude`, which sets
    /// `HitboxBehavior::BlockMouse` - `vendor/zed/crates/gpui/src/window.rs`'s `hit_test` stops
    /// walking hitboxes at the first one carrying it) is what makes this a real modal layer
    /// rather than a decorative one. Without it the scrim's `on_click` fired *and* the file-tree
    /// row underneath it took the same click - so dismissing the menu also opened whatever file
    /// happened to be under the cursor - and every row under the pointer still painted its
    /// `:hover` fill and its tooltip, because those read `Hitbox::is_hovered` directly and never
    /// consulted the click handlers at all. A `cx.stop_propagation()` in the scrim's own handler
    /// could only ever have fixed the click half; hover styling is not an event. The same
    /// `.occlude()` is on `Self::render_tree_delete_confirm`'s scrim, and this app already used
    /// the identical mechanism for the pane resize handles (`crate::root::resize`).
    pub(crate) fn render_tree_context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = self.tree_context_menu.clone();
        let macos = self.window_controls_style().is_macos();
        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::PLUS_MENU;
        let rows = menu
            .as_ref()
            .map(|menu| context_menu::menu_rows(&menu.target, self.tree_clipboard.is_some()))
            .unwrap_or_default();
        let origin_x = menu.as_ref().map(|menu| menu.origin_x).unwrap_or(0.0);
        let origin_y = menu.as_ref().map(|menu| menu.origin_y).unwrap_or(0.0);

        div()
            .id("tree-context-menu-scrim")
            .absolute()
            // Starts *below* the title bar, exactly like `crate::palette::render`'s own scrim
            // does and for the same reason - now a real one, since this layer `.occlude()`s.
            // A full-window occluding scrim swallows the window's own close/minimise/maximise
            // caption buttons and the title bar's drag region, so the window could not be closed
            // or moved while it was up. Reproduced against the real caption button by this
            // change's own adversarial audit.
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .occlude()
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.close_tree_context_menu(cx);
            }))
            // A right-click on the scrim must dismiss too - otherwise the next right-click
            // anywhere would land on the scrim and do nothing at all, which reads as a frozen
            // app rather than as a menu that is still open.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _event: &gpui::MouseDownEvent, _window, cx| {
                    this.close_tree_context_menu(cx);
                }),
            )
            .child(
                div()
                    .id("tree-context-menu")
                    .debug_selector(|| "tree-context-menu".to_string())
                    .absolute()
                    .left(px(origin_x))
                    // `origin_y` is window-space (it is clamped against `Window::bounds()`, from
                    // a window-space `MouseDownEvent::position`), but this panel is positioned
                    // relative to the scrim - which starts at `theme::band::TITLE_BAR`, not at
                    // the window top. Without this subtraction the menu paints a whole title bar
                    // too low. `context_menu_paints_at_its_clamped_window_space_origin` is the
                    // assertion that keeps the two in step.
                    .top(px(origin_y) - theme::band::TITLE_BAR)
                    .w(px(context_menu::MENU_WIDTH))
                    .py(px(context_menu::MENU_VERTICAL_PADDING / 2.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .shadow(vec![gpui::BoxShadow::new(
                        shadow_x,
                        shadow_y,
                        gpui::black().opacity(0.55),
                    )
                    .blur_radius(shadow_blur)])
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .children(rows.into_iter().map(|row| match row {
                        context_menu::MenuRow::Item(item) => {
                            self.render_tree_context_menu_row(item, macos, cx)
                        }
                        context_menu::MenuRow::Separator => render_menu_group_divider(),
                    })),
            )
    }

    /// One context-menu row. A disabled row is still drawn (so the menu's shape doesn't jump
    /// between right-clicks) but carries no click handler at all - not a handler that returns
    /// early, which would be a row that looks clickable and silently isn't.
    ///
    /// Speaks this app's own established dropdown-row language rather than one invented here.
    /// Every value below is `crate::work_surface::render::render_dropdown_menu_row`'s - the row
    /// shared by the tab strip's `+` menu and the title bar's File/Edit/View/Session/Help menus,
    /// and the closest thing this app has to a specified menu row (the design handoff's
    /// `revision/CHANGELOG.md` specifies that popover; it has no context-menu spec of its own).
    /// That function is deliberately *not* reused: its row is a fixed chip + label + sub-label +
    /// keycap quad, and a file-tree context menu has no honest chip glyph or secondary text for
    /// two of those four slots.
    ///
    /// Four values were off that language, and the first of them was the reported "the context
    /// menu has no hover state" bug outright:
    ///
    /// - **hover fill** was `theme::surface::ROW_HOVER` (`#15181b`) - the *exact* hex of
    ///   `theme::surface::PALETTE`, this panel's own background - so hovering a row painted
    ///   nothing at all. `theme::surface::PLUS_MENU_ROW_HOVER` (`#1d2226`) is the token that
    ///   exists for this; `theme::palette::ROW_HOVER`'s own docs record the identical trap for
    ///   the palette's rows.
    /// - **label colour and size** were `theme::text::BODY` at 11.0px, against the dropdown row's
    ///   `theme::text::HEADING` at 11.5px medium.
    /// - **gap** was 8px, against 9.
    /// - **disabled rows** were `theme::text::GHOST` and kept `cursor_pointer`'s absence but not
    ///   the explicit `cursor_default()`; the dropdown row uses `theme::text::GHOSTER` and sets
    ///   the cursor, so a row that cannot be run does not advertise itself as clickable.
    ///
    /// The destructive tint stays `theme::status::FAIL`, unchanged - `theme::button::DANGER_FG`
    /// is this app's destructive *button* pair (the rail's prune control, and now the delete
    /// confirmation's own confirm button), not its menu-row accent.
    fn render_tree_context_menu_row(
        &self,
        item: context_menu::MenuItem,
        macos: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let action = item.action;
        let keycaps = action
            .keystroke_spec()
            .map(|spec| keymap::resolve_combo(spec, macos))
            .unwrap_or_default();
        let color = if !item.enabled {
            theme::text::GHOSTER
        } else if action.is_destructive() {
            theme::status::FAIL
        } else {
            theme::text::HEADING
        };

        let mut row = div()
            .id(gpui::SharedString::from(format!(
                "tree-context-menu-{}",
                action.label()
            )))
            .debug_selector(move || format!("tree-context-menu-{}", action.label()))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(9.0))
            .w_full()
            .h(px(context_menu::MENU_ROW_HEIGHT))
            .px(px(10.0))
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(self.ui_text_size(11.5))
            .text_color(color)
            .child(div().flex_1().min_w_0().truncate().child(action.label()))
            .child(render_keycap_row(&keycaps, KeycapSize::Hint));

        if item.enabled {
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::PLUS_MENU_ROW_HOVER))
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.run_tree_menu_action(action, window, cx);
                }));
        } else {
            row = row.cursor_default();
            if let Some(reason) = item.disabled_reason {
                row = row.tooltip(text_tooltip(reason));
            }
        }

        row.into_any_element()
    }

    /// The delete confirmation (issue #19 §3: "Delete asks for confirmation and prefers the OS
    /// trash over a hard delete where available").
    ///
    /// A real modal panel with two explicit buttons rather than this app's other
    /// "click once to arm, click again to run" pattern
    /// (`crate::worktree_history::flow::AdeApp::request_discard_worktree`): that shape works for
    /// a button that stays in one place, but a context-menu row disappears the moment the menu
    /// closes, so there would be nothing left to click a second time. The confirm button's label
    /// and the sentence above it both come from the already-resolved
    /// [`crate::sidebar::tree_ops::PendingTreeDelete::mechanism`], so what is promised here and
    /// what actually runs cannot disagree.
    ///
    /// ## Design language
    ///
    /// The panel geometry is `crate::root::new_file::AdeApp::render_new_file_prompt`'s - this
    /// app's first centered modal and therefore the pattern to match, not to re-derive: scrim +
    /// `flex/items_center/justify_center`, panel `p(12)` `gap(8)` on
    /// `theme::surface::PALETTE`/`theme::border::POPOVER`/`theme::radius::CARD`, title SANS 11.5
    /// MEDIUM `theme::text::HEADING`, body SANS 10.5 `theme::text::DIM`. Three things that were
    /// genuinely off it are fixed here: the scrim was a raw `gpui::black()` literal rather than a
    /// theme token (now `crate::root::widgets::modal_scrim_bg`, shared with the New file prompt),
    /// the two buttons had no hover state at all, and the destructive one was tinted
    /// `theme::status::FAIL` - the rail's *status* red - rather than `theme::button::DANGER_FG`,
    /// the pair this app already uses for a destructive control. Both buttons now come from the
    /// one shared `crate::root::widgets::render_modal_button`.
    ///
    /// The scrim `.occlude()`s for the same real reason
    /// [`Self::render_tree_context_menu`]'s does - see that method's docs.
    pub(crate) fn render_tree_delete_confirm(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pending = self.tree_delete_confirm.clone();
        let name = pending
            .as_ref()
            .and_then(|pending| pending.path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let explanation = pending
            .as_ref()
            .map(|pending| pending.explanation())
            .unwrap_or_default();
        let confirm_label = pending
            .as_ref()
            .map(|pending| pending.confirm_label())
            .unwrap_or("Delete");

        div()
            .id("tree-delete-scrim")
            .absolute()
            // Starts *below* the title bar, exactly like `crate::palette::render`'s own scrim
            // does and for the same reason - now a real one, since this layer `.occlude()`s.
            // A full-window occluding scrim swallows the window's own close/minimise/maximise
            // caption buttons and the title bar's drag region, so the window could not be closed
            // or moved while it was up. Reproduced against the real caption button by this
            // change's own adversarial audit.
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            .occlude()
            .bg(modal_scrim_bg())
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.cancel_tree_delete(cx);
            }))
            .child(
                div()
                    .id("tree-delete-panel")
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .w(px(340.0))
                    .p(px(12.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(11.5))
                            .text_color(theme::text::HEADING)
                            .child(format!("Delete \"{name}\"?")),
                    )
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(10.5))
                            .text_color(theme::text::DIM)
                            .child(explanation),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                render_modal_button("tree-delete-cancel", "Cancel", false)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, _window, cx| {
                                            this.cancel_tree_delete(cx);
                                        },
                                    )),
                            )
                            .child(
                                render_modal_button("tree-delete-confirm", confirm_label, true)
                                    .on_click(cx.listener(
                                        |this, _event: &ClickEvent, _window, cx| {
                                            this.confirm_tree_delete(cx);
                                        },
                                    )),
                            ),
                    ),
            )
    }
}

/// The Changes list's footer 29. The README's spec text also mentions `] next file`, dropped
/// here since `]` isn't actually bound to anything (only `secondary-n` is - see
/// `crate::default_key_bindings`); advertising a dead shortcut is worse than a shorter,
/// accurate footer.
///
/// `text_size` is the caller's already-scaled [`AdeApp::ui_text_size`] value - this free
/// function has no `&self` to call that method through, so the one caller
/// ([`AdeApp::render_right_sidebar`]) computes and passes it in.
pub(in crate::sidebar) fn render_changes_footer(text_size: Pixels) -> impl IntoElement {
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

/// The Files tree's keyboard-hint footer - the counterpart to [`render_changes_footer`] beside
/// it, so switching between the two sidebar views with a diff loaded doesn't move the list under
/// the cursor. (The Changes view's *no-diff* arm still has no footer; that arm renders a single
/// message rather than a list, so there is nothing for a footer to sit under.)
///
/// This exists to answer a real, reported complaint: `Shift+F10` opens the tree's context menu
/// (issue #19 §2 required the menu to be reachable from the keyboard, so right-click alone was
/// never enough) but nothing in the product said so. A user's only encounter with it was a row in
/// Settings → Keybindings reading "Files tree: context menu", which explains what it does and not
/// why it exists or that it is the right-click equivalent. Two surfaces now do: that row's own
/// label (`crate::settings::state::action_label`), and this strip.
///
/// The shape is the hint strip the design handoff already specifies for
/// exactly this job - `design_handoff_jerry_ade/revision/Jerry.dc.html`'s own `diffHints` strip:
/// `height:28px ... padding:0 12px;background:#111316;border-top:1px solid #1c2023`, hints at
/// `gap:11`, each hint `gap:5`, hint-size keycaps, label `400 10px 'IBM Plex Sans'` in `#4a5057`.
/// Which is this app's
/// `theme::band::SURFACE_FOOTER`/`surface::FOOTER`/`border::INNER`/`text::PATH` and the
/// `KeycapSize::Hint` keycap it already had.
///
/// The keystrokes are resolved through [`keymap::resolve_combo`], the same per-platform
/// resolution the context menu's own row keycaps use - never a hard-coded keystroke string that
/// could drift from the real binding, which is also why the tooltip below names no key at all and
/// points at the keycaps instead. Both hints are for real, registered bindings in
/// `crate::default_key_bindings`, asserted by
/// `crate::sidebar::tree_ops`'s own
/// `the_file_tree_footer_only_advertises_real_registered_bindings`.
///
/// `live` is the other half of that honesty: both bindings are scoped
/// `"file-tree && !tree-editing && !tree-delete-confirm"`, so while an inline name editor or the
/// delete confirmation is open they genuinely do not fire, and the strip drops its hints rather
/// than advertising a dead shortcut. The band itself stays, so the tree doesn't jump 28px.
///
/// `text_size` - see [`render_changes_footer`]'s docs for why this takes an already-scaled value.
pub(in crate::sidebar) fn render_file_tree_footer(
    text_size: Pixels,
    macos: bool,
    live: bool,
) -> impl IntoElement {
    let hint = move |spec: &'static str, label: &'static str| {
        div()
            .flex()
            .items_center()
            .gap(px(5.0))
            .child(render_keycap_row(
                &keymap::resolve_combo(spec, macos),
                KeycapSize::Hint,
            ))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(text_size)
                    .text_color(theme::text::PATH)
                    .child(label),
            )
    };

    div()
        .id("file-tree-footer")
        .debug_selector(|| "file-tree-footer".to_string())
        .flex_none()
        .h(theme::band::SURFACE_FOOTER)
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(11.0))
        .border_t_1()
        .border_color(theme::border::INNER)
        .bg(theme::surface::FOOTER)
        .tooltip(text_tooltip(
            "Right-click a row for file actions. The keycaps here are the keyboard equivalents, \
             for the row currently selected in the tree.",
        ))
        .when(live, |el| {
            el.child(hint(FILE_TREE_CONTEXT_MENU_SPEC, "actions"))
                .child(hint(FILE_TREE_RENAME_SPEC, "rename"))
        })
}

/// The two `crate::keymap::resolve_combo` specs [`render_file_tree_footer`] advertises, named so
/// its own test can assert each one really is a registered binding rather than a plausible
/// string. `shift+F10` has no `mod` in it and so resolves identically on every platform; it still
/// goes through `resolve_combo` rather than being written out, so the glyphs match every other
/// keycap in the app.
pub(in crate::sidebar) const FILE_TREE_CONTEXT_MENU_SPEC: &str = "shift+F10";
pub(in crate::sidebar) const FILE_TREE_RENAME_SPEC: &str = "F2";

/// The file tree row's `▾`/`▸` caret, signaling a directory row is clickable/expandable,
/// distinct from the folder icon itself. Blank but still 8px wide for a file row, to keep
/// every row's icon column aligned.
///
/// `text_size` - see [`render_changes_footer`]'s docs for why this takes an already-scaled
/// value rather than computing it internally.
pub(in crate::sidebar) fn render_tree_caret(
    is_dir: bool,
    open: bool,
    text_size: Pixels,
) -> impl IntoElement {
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
pub(in crate::sidebar) fn render_folder_icon(open: bool) -> impl IntoElement {
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
/// `crate::sidebar::file_tree::lang_chip_for_name`'s selection logic (never an emoji, never a second,
/// independently maintained extension-matching guess).
pub(in crate::sidebar) fn render_lang_chip(chip: LangChip) -> impl IntoElement {
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
pub(in crate::sidebar) fn scrollable_sidebar_message(
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
pub(in crate::sidebar) fn render_moved_tag() -> impl IntoElement {
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
/// `crate::sidebar::changes::stat_bar_segments`'s real, unit-tested proportional allocation.
pub(in crate::sidebar) fn render_stat_bar(segments: [changes::StatSegment; 5]) -> impl IntoElement {
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
/// `crate::sidebar::file_tree::visible_entries` reports exactly the same rows either way. Only a real
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
    /// 500 (the since-removed `MAX_RENDERED_FILE_ENTRIES` cap) visible rows was built, laid out and painted
    /// on *every* frame - including all the ones below the fold - which measured, against real
    /// `gpui::FrameTiming` data on this repository's own tree, as ~145ms of a ~200ms
    /// `Window::draw`.
    #[gpui::test]
    fn a_file_tree_row_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        // Deliberately more rows than any plausible test viewport can show at
        // `theme::band::TREE_ROW` (22px) each, but fewer than
        // the since-removed `MAX_RENDERED_FILE_ENTRIES` cap, so this measured virtualization
        // alone even back when that cap still existed.
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
    /// content. A collapsed directory's children must be genuinely absent, and expanding it must
    /// genuinely bring them in - the state `crate::sidebar::file_tree::visible_entries` owns, now
    /// consulted from inside `uniform_list`'s row-builder rather than from an eager loop.
    ///
    /// Also the render-level proof of issue #18 §1's default: the very first assertion is that a
    /// directory's child does *not* paint before anything has been expanded.
    ///
    /// Honest about its own reach: with only two entries this exercises no virtualization at
    /// all, and it passes identically against the pre-fix eager loop. It is a guard on the
    /// tree's *content* surviving the rewrite, not evidence that the rewrite virtualizes -
    /// that is what the two "far below the viewport" tests and the scroll test are for.
    #[gpui::test]
    fn expanding_and_collapsing_a_directory_adds_and_removes_its_children(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir(repo.path().join("src")).expect("mkdir");
        fs::write(repo.path().join("src/only.rs"), "fn main() {}\n").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-row-src").is_some(),
            "the root-level directory row itself must paint"
        );
        assert!(
            cx.debug_bounds("file-tree-row-only.rs").is_none(),
            "nothing is expanded on first open, so a nested child must not paint"
        );

        let src_dir = repo.path().join("src");
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(src_dir.clone(), cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-row-only.rs").is_some(),
            "expanding must bring the child in - a virtualized list that caches its row set \
             without invalidating on `expanded_dirs` would fail exactly here"
        );

        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(src_dir, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-row-only.rs").is_none(),
            "collapsing again must remove it"
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
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
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

/// Real, end-to-end coverage for GitHub issue #18's persisted fold state: a live `AdeApp`, a
/// real fold-state file on disk, and a second `AdeApp` constructed against that same file
/// standing in for an app restart (the same "simulated reload" discipline
/// `crate::settings::render`'s keybinding-rebind test already established - nothing in this
/// codebase can restart the actual process mid-test).
#[cfg(test)]
mod fold_state_tests {
    use super::*;
    use crate::settings::store as settings_store;
    use crate::sidebar::fold_state::FoldState;
    use gpui::TestAppContext;
    use std::fs;
    use tempfile::TempDir;

    /// Opens an `AdeApp` with a *real*, temp-dir-scoped settings path - which is what gives it a
    /// real fold-state file (`fold_state::fold_state_path_for`), unlike
    /// `palette_focus_tests::open_test_app`'s deliberately unpersisted `None`.
    fn open_app_with_state_dir(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        open_app_with_state_dir_and_settings(
            cx,
            repo_path,
            settings_path,
            settings_store::Settings::default(),
        )
    }

    fn open_app_with_state_dir_and_settings(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
        settings: settings_store::Settings,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(repo_path, settings, Some(settings_path), window, cx)
        })
    }

    /// `src/app/` + `src/lib/` + a root-level file, the shape every test below expands into.
    fn seed_tree(repo: &TempDir) {
        fs::create_dir_all(repo.path().join("src/app")).expect("mkdir");
        fs::create_dir_all(repo.path().join("src/lib")).expect("mkdir");
        fs::write(repo.path().join("src/app/main.rs"), "fn main() {}\n").expect("write");
        fs::write(repo.path().join("src/lib/util.rs"), "pub fn u() {}\n").expect("write");
        fs::write(repo.path().join("README.md"), "hi\n").expect("write");
    }

    fn expanded_names(app: &AdeApp) -> Vec<String> {
        let mut names: Vec<String> = app
            .expanded_dirs
            .iter()
            .filter_map(|path| path.strip_prefix(&app.file_tree_root).ok())
            .map(|path| path.display().to_string())
            .collect();
        names.sort();
        names
    }

    /// §1: nothing is expanded on the first visit to a worktree, and the tree really only shows
    /// its root-level entries.
    #[gpui::test]
    fn a_worktree_opened_for_the_first_time_starts_fully_collapsed(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);

        let (app, cx) = open_app_with_state_dir(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.expanded_dirs.is_empty()));
        let visible = app.read_with(cx, |app, _| {
            file_tree::visible_entries(&app.file_tree, &app.expanded_dirs)
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(visible, vec!["src".to_string(), "README.md".to_string()]);
        assert!(cx.debug_bounds("file-tree-row-src").is_some());
        assert!(
            cx.debug_bounds("file-tree-row-main.rs").is_none(),
            "a file two levels down must not be showing before anything is expanded"
        );
    }

    /// §2's headline requirement: expand three folders, "quit", "relaunch" - the same three are
    /// open and nothing else. The reload reads the real file this app wrote, not an in-memory
    /// cache handed between the two instances.
    #[gpui::test]
    fn expanded_folders_are_restored_exactly_after_a_simulated_reload(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        let settings_path = state_dir.path().join("settings.toml");
        let fold_path = state_dir.path().join("file-tree-state.toml");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("src"), cx);
            app.toggle_dir_expanded(repo.path().join("src/app"), cx);
        });
        cx.run_until_parked();

        assert!(
            fold_path.exists(),
            "expanding a folder must be recorded on disk immediately - not on a clean exit, \
             which is exactly what this test never performs"
        );
        assert_eq!(
            FoldState::load_at(&fold_path)
                .expanded_dirs(repo.path())
                .len(),
            2
        );

        // The "relaunch": a brand-new `AdeApp` reading that same real file.
        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        cx.run_until_parked();

        assert_eq!(
            reloaded.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()],
            "the reloaded tree must restore exactly what was left open - `src/lib` was never \
             expanded and must stay closed"
        );
        assert!(
            cx.debug_bounds("file-tree-row-main.rs").is_some(),
            "the restored expansion must be visible in the real rendered tree, not just in state"
        );
        assert!(
            cx.debug_bounds("file-tree-row-util.rs").is_none(),
            "`src/lib` was never expanded, so its child must still be hidden after the reload"
        );
    }

    /// §2's identity requirement, at the app level: two worktrees with an identically-named
    /// `src/` share one fold-state file, and one's expansion must never open the other's.
    #[gpui::test]
    fn fold_state_from_one_worktree_never_leaks_into_another(cx: &mut TestAppContext) {
        let worktree_a = TempDir::new().expect("tempdir");
        let worktree_b = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&worktree_a);
        seed_tree(&worktree_b);
        let settings_path = state_dir.path().join("settings.toml");

        let (app_a, cx) =
            open_app_with_state_dir(cx, worktree_a.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        app_a.update(cx, |app, cx| {
            app.toggle_dir_expanded(worktree_a.path().join("src"), cx);
        });
        cx.run_until_parked();

        let (app_b, cx) =
            open_app_with_state_dir(cx, worktree_b.path().to_path_buf(), settings_path);
        cx.run_until_parked();

        assert!(
            app_b.read_with(cx, |app, _| app.expanded_dirs.is_empty()),
            "worktree B shares the relative path `src` with worktree A, and a fold-state entry \
             keyed by anything less than the real worktree path would open it here"
        );
        // And A's own state is genuinely still there - this must fail as a leak test, never as a
        // "nothing was ever persisted" test.
        let fold_path = state_dir.path().join("file-tree-state.toml");
        assert_eq!(
            FoldState::load_at(&fold_path)
                .expanded_dirs(worktree_a.path())
                .len(),
            1
        );
    }

    /// Two `jerry` processes (one per repository) share one fold-state file, and each holds a
    /// whole-file copy read at its own startup. A plain whole-file write would therefore make
    /// whichever instance saved last erase the other's worktree entirely.
    ///
    /// The ordering here is the whole point, and is what distinguishes this from
    /// `fold_state_from_one_worktree_never_leaks_into_another`: B starts *before* A's second
    /// write, so B's in-memory copy can never contain A's newer entry. Only a write that
    /// genuinely re-reads the file and merges can preserve it.
    #[gpui::test]
    fn a_second_instances_saves_never_erase_the_first_instances_worktree(cx: &mut TestAppContext) {
        let repo_a = TempDir::new().expect("tempdir");
        let repo_b = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo_a);
        seed_tree(&repo_b);
        let settings_path = state_dir.path().join("settings.toml");
        let fold_path = state_dir.path().join("file-tree-state.toml");

        let (app_a, cx) =
            open_app_with_state_dir(cx, repo_a.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();

        // Instance B starts here, snapshotting a file that knows nothing about A yet.
        let (app_b, cx) =
            open_app_with_state_dir(cx, repo_b.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();

        app_a.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo_a.path().join("src"), cx);
        });
        cx.run_until_parked();
        // ...and now B writes, from its stale whole-file snapshot.
        app_b.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo_b.path().join("src"), cx);
        });
        cx.run_until_parked();

        let on_disk = FoldState::load_at(&fold_path);
        assert_eq!(
            on_disk.expanded_dirs(repo_a.path()).len(),
            1,
            "the second instance's save must merge, not clobber - repository A's fold state is \
             gone here if the write is a plain whole-file one"
        );
        assert_eq!(on_disk.expanded_dirs(repo_b.path()).len(), 1);

        // And a real relaunch of A sees its own state, which is what the user actually notices.
        let (reloaded_a, cx) =
            open_app_with_state_dir(cx, repo_a.path().to_path_buf(), settings_path);
        cx.run_until_parked();
        assert_eq!(
            reloaded_a.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string()]
        );
    }

    /// §2: a folder that has since been deleted is dropped silently on the next load - no error
    /// surfaces, and the surviving entries are untouched.
    #[gpui::test]
    fn a_stale_entry_for_a_deleted_folder_is_pruned_silently(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        let settings_path = state_dir.path().join("settings.toml");
        let fold_path = state_dir.path().join("file-tree-state.toml");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("src"), cx);
            app.toggle_dir_expanded(repo.path().join("src/app"), cx);
            app.toggle_dir_expanded(repo.path().join("src/lib"), cx);
        });
        cx.run_until_parked();

        // An agent deletes one of them from underneath the running app.
        fs::remove_dir_all(repo.path().join("src/lib")).expect("remove");

        // The "relaunch" - which is also where the prune happens, against the freshly walked
        // tree.
        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        cx.run_until_parked();

        assert_eq!(
            reloaded.read_with(cx, |app, _| app.file_tree_error.clone()),
            None,
            "a stale fold-state entry must never surface as an error"
        );
        assert_eq!(
            reloaded.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()]
        );
        let on_disk = FoldState::load_at(&fold_path);
        assert_eq!(
            on_disk.expanded_dirs(repo.path()).len(),
            2,
            "the prune must be written back to the file, not just applied in memory"
        );
    }

    /// §2: a mid-session refresh of the same worktree (what an agent creating or deleting files
    /// causes, via `create_new_file`'s own reload) must not reset fold state.
    #[gpui::test]
    fn reloading_the_same_worktrees_tree_keeps_the_fold_state(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);

        let (app, cx) = open_app_with_state_dir(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("src"), cx);
            app.toggle_dir_expanded(repo.path().join("src/app"), cx);
        });
        cx.run_until_parked();

        // A file appears and another disappears underneath the running app, then the tree is
        // re-walked - neither touches any directory that was expanded.
        fs::write(repo.path().join("src/app/new.rs"), "//\n").expect("write");
        fs::remove_file(repo.path().join("README.md")).expect("remove");
        app.update(cx, |app, cx| {
            let root = app.file_tree_root.clone();
            app.load_file_tree(root, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()],
            "a refresh must re-render, not reset - the folders stay exactly as they were"
        );
        assert!(
            cx.debug_bounds("file-tree-row-new.rs").is_some(),
            "and the newly created file must really appear inside the still-expanded folder"
        );
    }

    /// §1: "collapse all" resets the tree *and* the saved state, in one step - so it survives a
    /// reload rather than springing back open.
    #[gpui::test]
    fn collapse_all_clears_both_the_tree_and_the_saved_state(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        let settings_path = state_dir.path().join("settings.toml");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("src"), cx);
            app.toggle_dir_expanded(repo.path().join("src/app"), cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("file-tree-row-main.rs").is_some());
        assert!(
            cx.debug_bounds("file-tree-collapse-all").is_some(),
            "the collapse-all control must really be in the rendered header"
        );

        app.update(cx, |app, cx| app.collapse_all_dirs(cx));
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.expanded_dirs.is_empty()));
        assert!(cx.debug_bounds("file-tree-row-main.rs").is_none());

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        cx.run_until_parked();
        assert!(
            reloaded.read_with(cx, |app, _| app.expanded_dirs.is_empty()),
            "collapse-all must have cleared the *saved* state too, or the folders would spring \
             back open on the next launch"
        );
    }

    /// §5: "reveal in tree", driven through the real command-palette open-file flow, expands
    /// every ancestor of the revealed file - and records those expansions exactly like manual
    /// ones, so they survive a reload.
    #[gpui::test]
    fn revealing_a_file_expands_its_ancestors_and_records_them(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        let settings_path = state_dir.path().join("settings.toml");
        let target = repo.path().join("src/app/main.rs");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-row-main.rs").is_none(),
            "precondition: the file to reveal starts hidden inside two collapsed folders"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_palette_file_result(target.clone(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()],
            "both ancestors - and only the ancestors - must be expanded"
        );
        assert!(
            cx.debug_bounds("file-tree-row-main.rs").is_some(),
            "the revealed file's row must really be showing"
        );
        // Reveal and open are one action, not two (the "Reveal in tree selects the file but does
        // not open it" report, and GitHub issue #15's "reveal + highlight the opened file in the
        // file tree"). Before that fix this branch expanded the ancestors and highlighted the row
        // while opening nothing at all.
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change.as_deref(),
                Some(Path::new("src/app/main.rs")),
                "revealing a file must also open its tab"
            );
            assert_eq!(app.selected_tree_path.as_deref(), Some(target.as_path()));
            assert_eq!(
                app.code_view,
                crate::code_surface::code_view::CodeView::File
            );
        });

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        cx.run_until_parked();
        assert_eq!(
            reloaded.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()],
            "a reveal is recorded like a manual expansion, so it must survive a reload"
        );
    }

    /// §5 again, through the other real entry point: opening a file directly (what
    /// go-to-definition does when it lands in a folder nobody has expanded) reveals it too.
    #[gpui::test]
    fn opening_a_file_directly_reveals_it_in_the_tree(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        let target = repo.path().join("src/app/main.rs");

        let (app, cx) = open_app_with_state_dir(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(target.clone(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()]
        );
        assert!(
            cx.debug_bounds("file-tree-row-main.rs").is_some(),
            "the selected row must actually be showing, or its selection highlight is invisible"
        );
    }

    /// §4: a directory with hundreds of entries renders *every* one of them - no truncation row,
    /// no cap - while staying virtualized (the row far below the viewport is never built).
    #[gpui::test]
    fn a_large_directory_renders_completely_with_no_truncation_row(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        fs::create_dir(repo.path().join("big")).expect("mkdir");
        for index in 0..800 {
            fs::write(repo.path().join(format!("big/f-{index:03}.txt")), "x\n").expect("write");
        }

        let (app, cx) = open_app_with_state_dir(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("big"), cx);
        });
        cx.run_until_parked();

        let visible = app.read_with(cx, |app, _| {
            file_tree::visible_entries(&app.file_tree, &app.expanded_dirs).len()
        });
        assert_eq!(
            visible, 801,
            "all 800 files plus the folder row itself are visible rows - the old 500-row cap \
             would have silently hidden 301 of them"
        );
        assert!(
            !app.read_with(cx, |app, _| app.file_tree_truncated),
            "800 entries is far below the configured load cap, so nothing was truncated"
        );
        assert!(
            cx.debug_bounds("file-tree-show-all").is_none(),
            "and no 'Show all entries' action may appear for a complete listing"
        );

        assert!(cx.debug_bounds("file-tree-row-f-000.txt").is_some());
        assert!(
            cx.debug_bounds("file-tree-row-f-799.txt").is_none(),
            "still virtualized: the last of 800 rows is far below the viewport and must never \
             become an element"
        );

        // The entry the old cap would have dropped entirely (#501 of 800) must be genuinely
        // reachable, not merely counted.
        let first_row = cx
            .debug_bounds("file-tree-row-f-000.txt")
            .expect("first row");
        cx.simulate_event(gpui::ScrollWheelEvent {
            position: first_row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-row-f-799.txt").is_some(),
            "scrolling to the bottom must reach the very last entry - proof the listing really \
             is complete rather than capped"
        );
    }

    /// §4's surviving load cap, and its honest replacement for the old silent cut-off: when the
    /// walk really does stop early, a real action appears that goes and loads more - and the cap
    /// it re-walks with is still a real cap, so a pathological tree can't be pulled into memory
    /// wholesale by one click.
    #[gpui::test]
    fn hitting_the_load_cap_offers_a_load_more_action_that_really_loads_more(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        for index in 0..40 {
            fs::write(repo.path().join(format!("f-{index:02}.txt")), "x\n").expect("write");
        }
        let mut settings = settings_store::Settings::default();
        settings.file_tree.max_entries = 10;

        let (app, cx) = open_app_with_state_dir_and_settings(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
            settings,
        );
        cx.run_until_parked();

        assert_eq!(app.read_with(cx, |app, _| app.file_tree.len()), 10);
        assert!(app.read_with(cx, |app, _| app.file_tree_truncated));
        assert!(
            cx.debug_bounds("file-tree-show-all").is_some(),
            "a walk that stopped early must say so with a real action, never silently"
        );

        app.update(cx, |app, cx| app.load_more_file_tree_entries(cx));
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree.len()),
            40,
            "the action must genuinely re-walk with a bigger budget - the capped walk never \
             collected these entries, so nothing else could have produced them"
        );
        assert!(!app.read_with(cx, |app, _| app.file_tree_truncated));
        assert!(cx.debug_bounds("file-tree-show-all").is_none());
        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_limit_override),
            Some(100),
            "the escape hatch raises the bound tenfold - it never removes it, or one click on a \
             pathological tree would pull the whole thing into memory"
        );
    }

    /// CRITICAL 1: "load more" must never *shrink* the budget. With a `max_entries` above the
    /// escalation ceiling, the old `saturating_mul(10).min(ceiling)` computed a smaller limit
    /// than the one already in force, so clicking "load more" would visibly remove rows from the
    /// tree.
    #[gpui::test]
    fn load_more_can_never_shrink_the_walk_budget(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        // Deliberately above `MAX_LOAD_MORE_ENTRIES`. `sanitize` clamps this on load from a real
        // file, so this is constructed directly - the handler must still be correct for it.
        let mut settings = settings_store::Settings::default();
        settings.file_tree.max_entries = file_tree::MAX_LOAD_MORE_ENTRIES * 5;

        let (app, cx) = open_app_with_state_dir_and_settings(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
            settings,
        );
        cx.run_until_parked();

        app.update(cx, |app, cx| app.load_more_file_tree_entries(cx));
        cx.run_until_parked();

        let effective = app.read_with(cx, |app, _| {
            app.file_tree_limit_override
                .unwrap_or(app.settings.file_tree.max_entries)
        });
        assert!(
            effective >= file_tree::MAX_LOAD_MORE_ENTRIES * 5,
            "the effective walk budget went *down* from {} to {effective} - one click on \
             `load more` would remove rows from the tree",
            file_tree::MAX_LOAD_MORE_ENTRIES * 5
        );
    }

    /// CRITICAL 1, other half: once the budget is at the ceiling and the walk is *still*
    /// truncated, the row must stop being an action. Clicking it would re-walk a whole ceiling's
    /// worth of entries to produce byte-identical rows.
    ///
    /// The at-the-ceiling state is set directly rather than reached by walking: producing it
    /// honestly needs `MAX_LOAD_MORE_ENTRIES` real files on disk. Everything asserted *from* that
    /// precondition is real behaviour - what renders, and what the handler does.
    #[gpui::test]
    fn at_the_ceiling_the_row_stops_being_a_button(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);

        let (app, cx) = open_app_with_state_dir(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.file_tree_truncated = true;
            app.file_tree_limit_override = Some(file_tree::MAX_LOAD_MORE_ENTRIES);
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-show-all").is_none(),
            "at the ceiling there is nothing left to load, so the clickable row must be gone - \
             a button that re-walks a ceiling's worth of entries for identical results is worse \
             than no button"
        );

        let before = app.read_with(cx, |app, _| app.file_tree_limit_override);
        app.update(cx, |app, cx| app.load_more_file_tree_entries(cx));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_limit_override),
            before,
            "and the handler itself must be a no-op at the ceiling, not just unreachable"
        );
    }

    /// CRITICAL 2: the canonicalized worktree key is resolved once per real root change and
    /// cached, because `worktree_key` calls the blocking `std::fs::canonicalize` and the callers
    /// are clicks and per-ancestor reveals. What's asserted here is the part that would actually
    /// break if the cache were wrong: reaching a worktree through a symlink must record against
    /// the *canonical* path, and a reveal through that symlink must still persist.
    #[cfg(unix)]
    #[gpui::test]
    fn the_cached_worktree_key_is_canonical_and_records_through_a_symlink(cx: &mut TestAppContext) {
        let real = TempDir::new().expect("tempdir");
        let link_parent = TempDir::new().expect("tempdir");
        seed_tree(&real);
        let link = link_parent.path().join("linked-worktree");
        std::os::unix::fs::symlink(real.path(), &link).expect("symlink");
        let state_dir = TempDir::new().expect("tempdir");

        let (app, cx) =
            open_app_with_state_dir(cx, link.clone(), state_dir.path().join("settings.toml"));
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.fold_state_root_key.clone()),
            crate::sidebar::fold_state::worktree_key(&link),
            "the cached key must be the canonicalized one, so the same worktree reached through \
             a symlink and through its real path is one entry rather than two"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_palette_file_result(link.join("src/app/main.rs"), window, cx);
        });
        cx.run_until_parked();

        // Reopening against the *real* path must see what was recorded through the symlink.
        let (reloaded, cx) = open_app_with_state_dir(
            cx,
            real.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();
        assert_eq!(
            reloaded.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string(), "src/app".to_string()],
            "a symlinked and a direct open of one worktree must share one fold-state entry"
        );
    }

    /// CRITICAL 4: a write that fails must not silently drop the change. The pending flag is
    /// cleared *before* the write, so without an explicit re-queue on error the user's
    /// expand/collapse is lost with only a log line - directly contradicting this feature's
    /// "recorded immediately" claim.
    ///
    /// The failure is real, not injected: the settings path's parent is a regular *file*, so the
    /// `create_dir_all` inside `FoldState::save_at` fails on every attempt.
    #[gpui::test]
    fn a_failed_fold_state_write_is_requeued_rather_than_dropped(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        seed_tree(&repo);
        let blocker = state_dir.path().join("not-a-directory");
        fs::write(
            &blocker,
            "this is a file, so nothing can be created inside it",
        )
        .expect("write");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), blocker.join("settings.toml"));
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("src"), cx);
        });
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.fold_state_save_pending),
            "after a failing write the change must still be pending a retry - clearing the flag \
             before the write and not restoring it on error loses the change outright"
        );
        assert!(
            app.read_with(cx, |app, _| app.fold_state_save_running),
            "and the writer loop must still be alive to perform that retry"
        );
        assert!(
            app.read_with(cx, |app, _| app.expanded_dirs.len()) == 1,
            "the live tree keeps the expansion regardless - a failing disk is not a reason to \
             refuse to open a folder"
        );
    }

    /// A truncated walk must never be used as evidence that the directories it never reached are
    /// gone - pruning against it would silently, and permanently, destroy good state. Driven
    /// through a walk that genuinely truncates, not by hand-setting the flag.
    #[gpui::test]
    fn a_truncated_walk_never_prunes_fold_state(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        // Directories sort first and then alphabetically, so a 1-entry budget reaches `aaa` and
        // stops - `zzz`, the one whose expansion is recorded, is never seen by the capped walk.
        fs::create_dir(repo.path().join("aaa")).expect("mkdir");
        fs::create_dir(repo.path().join("zzz")).expect("mkdir");
        fs::write(repo.path().join("zzz/inside.txt"), "x\n").expect("write");
        let settings_path = state_dir.path().join("settings.toml");
        let fold_path = state_dir.path().join("file-tree-state.toml");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("zzz"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            FoldState::load_at(&fold_path)
                .expanded_dirs(repo.path())
                .len(),
            1
        );

        let mut capped = settings_store::Settings::default();
        capped.file_tree.max_entries = 1;
        let (reloaded, cx) = open_app_with_state_dir_and_settings(
            cx,
            repo.path().to_path_buf(),
            settings_path,
            capped,
        );
        cx.run_until_parked();

        assert!(
            reloaded.read_with(cx, |app, _| app.file_tree_truncated),
            "precondition: the walk really must have stopped early, from the real cap - not \
             from a flag this test set itself"
        );
        assert!(
            reloaded.read_with(cx, |app, _| !app
                .file_tree
                .iter()
                .any(|entry| entry.name == "zzz")),
            "precondition: the expanded directory really must be absent from the capped listing"
        );

        assert_eq!(
            FoldState::load_at(&fold_path)
                .expanded_dirs(repo.path())
                .len(),
            1,
            "an incomplete listing is not evidence that a folder is gone"
        );
    }
}

/// GitHub issue #18 §3, verified where it actually matters: against real painted geometry in a
/// real virtualized list, not against the pure `indent_guide_x` arithmetic alone (which
/// `crate::sidebar::file_tree`'s own unit tests already cover).
///
/// The failure mode the issue names - "gaps or misaligned segments as rows recycle while
/// scrolling" - is only observable this way: every assertion below reads
/// `VisualTestContext::debug_bounds`, i.e. the bounds GPUI genuinely laid the guide out at, and
/// the recycling test re-asserts them *after* a real scroll event has forced `uniform_list` to
/// rebuild its rows from a different starting index.
#[cfg(test)]
mod indent_guide_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::fs;
    use tempfile::TempDir;

    fn close_enough(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0
    }

    /// `a/b/c/deep.txt`, with the whole chain expanded, plus a sibling file at each level.
    fn open_deep_tree<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        fs::create_dir_all(repo.path().join("a/b/c")).expect("mkdir");
        fs::write(repo.path().join("a/b/c/deep.txt"), "x\n").expect("write");
        fs::write(repo.path().join("a/b/mid.txt"), "x\n").expect("write");
        fs::write(repo.path().join("a/top.txt"), "x\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("a"), cx);
            app.toggle_dir_expanded(repo.path().join("a/b"), cx);
            app.toggle_dir_expanded(repo.path().join("a/b/c"), cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    /// One guide per nesting level, each at exactly its level's chevron offset from the row's
    /// own left edge, and none at all for a root-level row.
    #[gpui::test]
    fn each_row_draws_one_guide_per_level_aligned_with_that_levels_chevron(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        let (_app, cx) = open_deep_tree(cx, &repo);

        assert!(
            cx.debug_bounds("file-tree-guide-idle-a-0").is_none(),
            "a root-level row has no ancestors, so it must draw no guide at all"
        );

        let row = cx
            .debug_bounds("file-tree-row-deep.txt")
            .expect("the deepest row must paint");
        // Literal selectors, not `format!`: `debug_bounds` takes a `&'static str`.
        for (level, selector) in [
            (0usize, "file-tree-guide-idle-deep.txt-0"),
            (1, "file-tree-guide-idle-deep.txt-1"),
            (2, "file-tree-guide-idle-deep.txt-2"),
        ] {
            let guide = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("a depth-3 row must draw a guide for level {level}"));
            assert!(
                close_enough(
                    f32::from(guide.origin.x - row.origin.x),
                    file_tree::indent_guide_x(level)
                ),
                "level {level}'s guide must sit under that level's expand chevron: expected \
                 {:?} from the row's left edge, got {:?}",
                file_tree::indent_guide_x(level),
                f32::from(guide.origin.x - row.origin.x)
            );
        }
        assert!(
            cx.debug_bounds("file-tree-guide-idle-deep.txt-3").is_none(),
            "a depth-3 row must not draw a fourth guide"
        );
    }

    /// The "no gaps" half: a guide spans its row's full height, and consecutive rows' segments
    /// for the same level meet exactly - which is what makes them read as one continuous line
    /// rather than a dashed one.
    #[gpui::test]
    fn guides_on_consecutive_rows_join_with_no_gap(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let (_app, cx) = open_deep_tree(cx, &repo);

        // The three consecutive rows `c`, `deep.txt`, `mid.txt` (in that render order - `c`'s
        // own child comes between it and its sibling), all of which draw a level-1 guide.
        let segments: Vec<gpui::Bounds<Pixels>> = [
            "file-tree-guide-idle-c-1",
            "file-tree-guide-idle-deep.txt-1",
            "file-tree-guide-idle-mid.txt-1",
        ]
        .into_iter()
        .map(|selector| {
            cx.debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} must paint"))
        })
        .collect();
        let row = cx.debug_bounds("file-tree-row-c").expect("the `c` row");

        assert!(
            close_enough(
                f32::from(segments[0].size.height),
                f32::from(row.size.height)
            ),
            "a guide must span its row's whole height, or consecutive rows leave a visible gap"
        );
        for pair in segments.windows(2) {
            let (upper, lower) = (pair[0], pair[1]);
            assert!(
                close_enough(f32::from(upper.origin.x), f32::from(lower.origin.x)),
                "the same level's guide must be at the same x on every row"
            );
            assert!(
                close_enough(
                    f32::from(upper.origin.y + upper.size.height),
                    f32::from(lower.origin.y)
                ),
                "one row's guide must end exactly where the next row's begins: {:?} vs {:?}",
                f32::from(upper.origin.y + upper.size.height),
                f32::from(lower.origin.y)
            );
        }
    }

    /// The real risk this whole approach was chosen to eliminate: after a scroll, `uniform_list`
    /// rebuilds its rows from a different start index, reusing element slots. A guide drawn from
    /// anything other than the row's own depth would land on the wrong row here.
    #[gpui::test]
    fn guides_stay_aligned_with_their_own_rows_after_the_list_recycles(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir_all(repo.path().join("nested/inner")).expect("mkdir");
        // Enough rows inside the nested folder to fill several viewports, so scrolling genuinely
        // recycles row elements rather than merely translating them.
        for index in 0..300 {
            fs::write(
                repo.path().join(format!("nested/inner/f-{index:03}.txt")),
                "x\n",
            )
            .expect("write");
        }
        let (_app, cx) = {
            let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
            cx.run_until_parked();
            app.update(cx, |app, cx| {
                app.toggle_dir_expanded(repo.path().join("nested"), cx);
                app.toggle_dir_expanded(repo.path().join("nested/inner"), cx);
            });
            cx.run_until_parked();
            (app, cx)
        };

        let first_row = cx
            .debug_bounds("file-tree-row-f-000.txt")
            .expect("the first nested row must paint");
        assert!(
            cx.debug_bounds("file-tree-row-f-299.txt").is_none(),
            "precondition: the last row is below the viewport, so reaching it must recycle rows"
        );

        cx.simulate_event(gpui::ScrollWheelEvent {
            position: first_row.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.0), px(-100_000.0))),
            modifiers: gpui::Modifiers::default(),
            touch_phase: gpui::TouchPhase::Moved,
        });
        cx.run_until_parked();

        let recycled_row = cx
            .debug_bounds("file-tree-row-f-299.txt")
            .expect("the last row must materialize after scrolling");
        assert!(
            cx.debug_bounds("file-tree-guide-idle-f-000.txt-0")
                .is_none(),
            "the row that scrolled out of view must take its guides with it - a leftover guide \
             here would be a segment painted over an unrelated row"
        );

        for (level, selector) in [
            (0usize, "file-tree-guide-idle-f-299.txt-0"),
            (1, "file-tree-guide-idle-f-299.txt-1"),
        ] {
            let guide = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("recycled row must still draw its level-{level} guide"));
            assert!(
                close_enough(
                    f32::from(guide.origin.x - recycled_row.origin.x),
                    file_tree::indent_guide_x(level)
                ),
                "after recycling, level {level}'s guide must still be at that level's offset"
            );
            assert!(
                close_enough(f32::from(guide.origin.y), f32::from(recycled_row.origin.y)),
                "after recycling, the guide must still sit on its own row, not a neighbour's"
            );
            assert!(
                close_enough(
                    f32::from(guide.size.height),
                    f32::from(recycled_row.size.height)
                ),
                "after recycling, the guide must still span its row's full height"
            );
        }
    }

    /// The optional half of §3: the guides along the selected file's ancestor chain are drawn in
    /// the highlighted colour, and only those. The colour itself isn't observable from a test,
    /// so this asserts on which of the two real branches each guide took (see the guide's own
    /// `debug_selector`) - which is what would actually break if the condition were inverted.
    #[gpui::test]
    fn only_the_selected_files_ancestor_chain_is_highlighted(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        // A second branch at the same depth as `a/b`, so there is a row whose guides must stay
        // idle while the selection's chain is active.
        fs::create_dir_all(repo.path().join("a/other")).expect("mkdir");
        fs::write(repo.path().join("a/other/elsewhere.txt"), "x\n").expect("write");
        let (app, cx) = open_deep_tree(cx, &repo);
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("a/other"), cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-guide-idle-deep.txt-0").is_some(),
            "precondition: with nothing selected every guide is idle"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("a/b/c/deep.txt"), window, cx);
        });
        cx.run_until_parked();

        for selector in [
            "file-tree-guide-active-deep.txt-0",
            "file-tree-guide-active-deep.txt-1",
            "file-tree-guide-active-deep.txt-2",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector}: every guide on the selected file's own row is part of its \
                 ancestor chain"
            );
        }
        // `a/other/elsewhere.txt` shares only `a` with the selection.
        assert!(
            cx.debug_bounds("file-tree-guide-active-elsewhere.txt-0")
                .is_some(),
            "the shared `a` ancestor is on the chain"
        );
        assert!(
            cx.debug_bounds("file-tree-guide-idle-elsewhere.txt-1")
                .is_some(),
            "`a/other` is not on the selected file's chain, so its guide stays idle"
        );
        assert!(
            cx.debug_bounds("file-tree-guide-active-elsewhere.txt-1")
                .is_none(),
            "and it must not also be drawn as active"
        );
    }

    /// Collapsing a folder removes its children's guides along with their rows - the guides are
    /// pure functions of the rows that are actually showing, with no independently-tracked state
    /// that could survive them.
    #[gpui::test]
    fn collapsing_removes_the_hidden_rows_guides_too(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let (app, cx) = open_deep_tree(cx, &repo);
        assert!(cx.debug_bounds("file-tree-guide-idle-deep.txt-2").is_some());

        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("a/b"), cx);
        });
        cx.run_until_parked();

        assert!(cx.debug_bounds("file-tree-row-deep.txt").is_none());
        assert!(
            cx.debug_bounds("file-tree-guide-idle-deep.txt-2").is_none(),
            "a hidden row's guides must be gone with it"
        );
        assert!(
            cx.debug_bounds("file-tree-guide-idle-b-0").is_some(),
            "the still-visible `b` row keeps its own level-0 guide"
        );
    }
}
