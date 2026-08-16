use super::*;
use crate::keymap;
use crate::root::plural;
use crate::root::scrollbar;
use crate::root::widgets::{
    hover_bg, menu_popover_chrome, render_committed_tag, render_disclosure_caret,
    render_keycap_row, render_sidebar_message, render_status_letter, text_tooltip, KeycapSize,
    SimpleInput, SimpleInputCaret, TextFieldHandle,
};
use crate::settings::widgets::ChoiceOption;
use crate::worktree_history::flow as worktree_history;
use gpui::SharedString;
use std::time::Instant;

/// How much extra the Changes panel's `gpui::list` measures above and below the viewport.
pub(crate) const CHANGES_LIST_OVERDRAW: gpui::Pixels = px(48.0);

/// git's own conventional short form of a commit id, for a label that only has room for one.
/// Never a hand-picked width elsewhere in this file - one length, defined once.
/// The commit composer's own [`TextFieldHandle`] - what click/drag selection and GitHub issue
/// #336's four clipboard/select-all actions act on. No `on_changed`: the composer's buttons all
/// read `AdeApp::commit_message` directly at render time.
fn commit_message_handle() -> TextFieldHandle {
    TextFieldHandle::new(|app: &mut AdeApp| Some(&mut app.commit_message))
}

/// The file tree's inline New File / New Folder / Rename name editor's own field handle. `None`
/// whenever no editor is open, and its `on_changed` clears the stale rejection hint exactly as
/// `crate::sidebar::tree_ops::AdeApp::handle_tree_key_down` does after a keystroke.
fn tree_inline_edit_handle() -> TextFieldHandle {
    TextFieldHandle::new(|app: &mut AdeApp| {
        app.tree_inline_edit.as_mut().map(|edit| &mut edit.name)
    })
    .on_changed(|app: &mut AdeApp, _cx| {
        if let Some(edit) = app.tree_inline_edit.as_mut() {
            edit.error = None;
        }
    })
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

impl AdeApp {
    /// Switches which data source the right sidebar shows. Switching *to* the Changes view
    /// always recomputes the diff (`Self::refresh_diff`, not just `cx.notify()`) rather than
    /// showing whatever was last loaded - a stale snapshot from when the worktree was first
    /// selected would silently hide changes an agent just made.
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
                let fallback = self.focus_fallback_handle();
                restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
            }
            // 1 again, one level removed, and invisible to the check above: an *overlay* can be
            // holding the tree's handle as its own return target while the overlay itself has
            // focus - which is exactly the state the palette's own "Cycle Right Panel"
            // command runs in, since the palette is focused and the tree is not. Closing the
            // palette afterwards would then restore focus onto a handle this very branch has
            // just unrendered. Swept from every overlay rather than only the palette, so a
            // future overlay reaching this path doesn't reintroduce it. Reproduced by this
            // change's own adversarial audit; see `OverlayFocus::forget_target`.
            self.palette_focus.forget_target(&self.tree_focus_handle);
            self.settings_focus.forget_target(&self.tree_focus_handle);
            self.code_focus.forget_target(&self.tree_focus_handle);
        }
        if view != RightSidebarView::Search {
            // The same dangling-focus invariant as the Files block above, for GitHub issue #162's
            // four fields: leaving Search unrenders every one of their `track_focus` nodes, so a
            // handle still focused here would leave `Window::focus` pointing at a `FocusId` no
            // rendered frame can resolve - which silently kills every context-scoped binding and
            // the focused terminal's own key handling until the next click. Swept from the
            // overlays too, for the reason the Files block's own `forget_target` sweep records:
            // the palette can be the focused surface while holding a search field as its return
            // target, and closing it would restore focus onto a node this branch just unrendered.
            for field in crate::search::state::SearchField::ALL {
                let handle = self.search.focus_handle(field).clone();
                if handle.is_focused(window) {
                    let fallback = self.focus_fallback_handle();
                    restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
                }
                self.palette_focus.forget_target(&handle);
                self.settings_focus.forget_target(&handle);
                self.code_focus.forget_target(&handle);
            }
        }
        if view != RightSidebarView::Changes {
            // GitHub issue #176, the mirror image of the block above: leaving Changes unrenders
            // the commit composer, but `commit_menu_open` used to stay latched `true` - so coming
            // back to Changes popped the `▾` popover open again on its own, with no click.
            self.commit_menu_open = false;
        }
        self.right_sidebar_view = view;
        if view == RightSidebarView::Changes {
            self.refresh_diff(cx);
        }
        // `refresh_diff` (unlike `load_diff`) never touches any field the render path reads
        // synchronously - it only writes once its background task lands, so it never calls
        // `cx.notify()` itself. `self.right_sidebar_view` above did change, though, and every
        // other branch this function takes (the `Files`/`Search` unrender blocks, the `else`
        // this used to be) needs the same repaint - so this now unconditionally notifies once,
        // rather than one call in the `Changes` arm (via `load_diff`) and a second, separate one
        // in every other arm.
        cx.notify();
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

    /// Toggles a file's staged state (Revision R12 §5: the checkbox **is** staging) - the
    /// Changes row checkbox's click handler. `Self::render_change_row` stops propagation at the
    /// call site so checking a box never also opens that file's diff.
    pub(in crate::sidebar) fn toggle_staged(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let should_stage = !self.staged_files.remove(&path);
        if should_stage {
            self.staged_files.insert(path.clone());
        }
        self.changes_row_error = None;
        cx.notify();

        let worktree_path = self.diff_root.clone();
        let git_path = path.clone();
        let revert_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if should_stage {
                        wt_core::stage::stage_path(&worktree_path, &git_path)
                    } else {
                        wt_core::stage::unstage_path(&worktree_path, &git_path)
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(err) = result {
                    // Revert the optimistic flip - the real index never actually changed, so
                    // the checkbox must not keep claiming it did.
                    if should_stage {
                        this.staged_files.remove(&revert_path);
                    } else {
                        this.staged_files.insert(revert_path.clone());
                    }
                    let verb = if should_stage { "stage" } else { "unstage" };
                    this.changes_row_error =
                        Some((revert_path, format!("failed to {verb}: {err}")));
                    cx.notify();
                }
            });
        });
        // Two different rows' checkboxes clicked in quick succession are independent real git
        // operations - see `Self::_stage_tasks`'s own docs for why this can't be a single
        // `Option<Task<()>>` slot.
        self._stage_tasks.push(task);
    }

    /// The real, honest surface for a failed real staging/unstaging call
    /// ([`Self::changes_row_error`]) - the Changes-panel sibling of [`Self::tree_op_error`]'s own
    /// render site, next to the composer the failed checkbox lives above rather than buried in
    /// the log.
    pub(in crate::sidebar) fn render_changes_row_error(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let (_, message) = self.changes_row_error.clone()?;
        // GitHub issue #128.
        let row = hover_bg(
            div()
                .id("changes-row-error")
                .debug_selector(|| "changes-row-error".to_string())
                .flex_none()
                .w_full()
                .px(px(10.0))
                .py(px(5.0))
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(10.0))
                .text_color(theme::status::FAIL)
                .cursor_pointer(),
            theme::surface::ROW_HOVER,
        );
        Some(
            row.tooltip(text_tooltip("Click to dismiss"))
                .child(message)
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.changes_row_error = None;
                    cx.notify();
                })),
        )
    }

    /// The commit composer's primary action (Revision R12 §5) - a real `git add -- <staged
    /// paths>` + `git commit` (`wt_core::undo::commit_paths`) on [`Self::diff_root`] (the
    /// worktree the currently-shown diff/staged set belongs to), using
    /// [`Self::staged_commit_message`] - the user's own typed text; there is no auto-drafted
    /// fallback (see that method's own docs), so an empty message is as much a no-op as nothing
    /// staged. A genuine no-op - never a clickable-looking op that silently does nothing - with
    /// nothing staged, no message, or while another worktree-history operation is already in
    /// flight (shares
    /// [`Self::worktree_history_op_in_flight`] with `Keep all changes`/`Discard worktree`/`Undo`/
    /// `Redo` - see `worktree_history::flow`'s own module docs for why one flag is enough
    /// discipline here). On success, clears exactly the committed `paths` (captured once, up
    /// front, before the async git work starts) from [`Self::staged_files`] - any *other* path
    /// staged or unstaged during the in-flight window is untouched, since the removal loop only
    /// ever iterates that fixed snapshot. This is unconditional for the paths that *were*
    /// committed, though: [`Self::staged_files`] is plain UI bookkeeping with no per-path
    /// staged/committed history, so if a user un-stages and re-stages one of the very paths this
    /// call just committed while it was still in flight (the staging checkbox itself isn't
    /// gated on [`Self::worktree_history_op_in_flight`]), that path is still cleared once the
    /// commit finishes - there is no way to tell that re-staging apart from it never having been
    /// touched. Reloads the diff so it reflects the real post-commit state. That reload does
    /// **not**
    /// generally drop the just-committed files out of the Changes list: `wt_core::diff`'s own
    /// docs are explicit that `diff_against_base` diffs the working tree against the
    /// **merge-base with the default branch**, not against the previous commit, so a file
    /// committed on a feature branch that still differs from that branch's content keeps
    /// showing up - correctly, since it is still an uncommitted-relative-to-`main` change worth
    /// reviewing, only now with its content latched into a real commit instead of sitting
    /// uncommitted.
    pub(in crate::sidebar) fn commit_staged_files(&mut self, cx: &mut Context<Self>) {
        if self.worktree_history_op_in_flight.is_some() {
            return;
        }
        let Some(paths) = self.staged_uncommitted_paths() else {
            return;
        };
        let message = self.staged_commit_message();
        if message.trim().is_empty() {
            return;
        }
        let worktree_path = self.diff_root.clone();
        let branch_display = self.branch_display_for(&worktree_path);
        let file_count = paths.len();

        self.worktree_history_op_in_flight = Some(worktree_history::WorktreeHistoryOpKind::Commit);
        self.worktree_history_status = Some(format!(
            "committing {} in {branch_display}\u{2026}",
            plural::count(file_count, "file", None)
        ));
        cx.notify();

        let commit_paths = paths.clone();
        let commit_worktree_path = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    wt_core::undo::commit_paths(&commit_worktree_path, &commit_paths, &message)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.worktree_history_op_in_flight = None;
                match result {
                    Ok(_outcome) => {
                        this.worktree_history_status = Some(format!(
                            "committed {} in {branch_display}",
                            plural::count(file_count, "file", None)
                        ));
                        for path in &paths {
                            this.staged_files.remove(path);
                        }
                        this.load_diff(worktree_path.clone(), cx);
                    }
                    Err(err) => {
                        this.worktree_history_status = Some(format!("commit failed: {err}"));
                    }
                }
                cx.notify();
            });
        });
        self._worktree_history_task = Some(task);
    }

    /// The staged subset of the **uncommitted** scope, as real paths - `None` when nothing is
    /// staged, which is the one shape every commit-composer action's guard wants.
    pub(in crate::sidebar) fn staged_uncommitted_paths(&self) -> Option<Vec<PathBuf>> {
        let diff = self.uncommitted_diff.loaded()?;
        let staged = changes::staged_subset(&diff.files, &self.staged_files);
        if staged.is_empty() {
            return None;
        }
        Some(staged.iter().map(|file| file.path.clone()).collect())
    }

    /// The message every commit path in this composer writes - the user's own typed text,
    /// verbatim, and nothing else.
    pub(in crate::sidebar) fn staged_commit_message(&self) -> String {
        self.commit_message.as_str().to_string()
    }

    /// Click-to-focus for the commit message field - moves real keyboard focus onto it. Nothing
    /// to seed any more (see [`Self::staged_commit_message`]'s own docs): the field starts empty
    /// and stays whatever the user typed into it.
    pub(in crate::sidebar) fn focus_commit_message(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.commit_message_focus_handle, cx);
        cx.notify();
    }

    /// Mirrors [`crate::rail::render::AdeApp::handle_filter_key_down`] exactly - see that
    /// function's own docs for the modifier guard and the real undo/redo wiring this shares.
    pub(in crate::sidebar) fn handle_commit_message_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        // This is still what lets `mod+enter` reach the composer's own commit action from inside
        // the field rather than being typed into it.
        let Some(modifiers) =
            crate::root::widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        if self.commit_message.handle_editing_key(
            &keystroke.key,
            keystroke.key_char.as_deref(),
            modifiers,
            Instant::now(),
        ) {
            cx.notify();
            cx.stop_propagation();
        }
    }

    pub(in crate::sidebar) fn handle_commit_message_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.commit_message.undo() {
            cx.notify();
        }
    }

    pub(in crate::sidebar) fn handle_commit_message_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.commit_message.redo() {
            cx.notify();
        }
    }

    /// Whether `action` can really run right now, and - when it cannot - the reason, stated on the
    /// row itself.
    pub(in crate::sidebar) fn commit_menu_availability(
        &self,
        action: CommitMenuAction,
    ) -> Result<(), String> {
        if self.worktree_history_op_in_flight.is_some() {
            return Err("another git operation is still running".to_string());
        }
        let staged = self.staged_uncommitted_paths().map(|paths| paths.len());
        // A loaded uncommitted scope is also the proof that `HEAD` is born: `diff_against_head`
        // reports `Ok(None)` for an unborn `HEAD`, which `load_diff` turns into an `Error`.
        let Some(uncommitted) = self.uncommitted_diff.loaded() else {
            return Err("the working tree has not been read yet".to_string());
        };
        // Every action below except amending writes `Self::staged_commit_message` verbatim
        // (`AmendLastCommit` keeps the commit it folds into's own existing message) - with no
        // more auto-drafted fallback, a real message is a real precondition now, the same way
        // "nothing staged" already is.
        let has_message = !self.staged_commit_message().trim().is_empty();
        match action {
            CommitMenuAction::CommitAndPush => {
                if staged.is_none() {
                    return Err("nothing staged".to_string());
                }
                if !has_message {
                    return Err("write a commit message first".to_string());
                }
                if self.composer_branch().is_none() {
                    return Err("HEAD is detached, so there is no branch to push".to_string());
                }
                Ok(())
            }
            CommitMenuAction::CommitAllFiles => {
                if uncommitted.files.is_empty() {
                    return Err("nothing uncommitted to commit".to_string());
                }
                if !has_message {
                    return Err("write a commit message first".to_string());
                }
                Ok(())
            }
            CommitMenuAction::AmendLastCommit => {
                if staged.is_none() {
                    return Err("nothing staged to fold into the last commit".to_string());
                }
                // Amending needs a commit to amend. The Commits scope knows whether this branch
                // has one of its own; with no base branch it cannot say, and git's own refusal is
                // then the honest answer rather than a guess made here.
                if self.branch_commits.loaded().is_some_and(|commits| {
                    commits.base_branch.is_some() && commits.commits.is_empty()
                }) {
                    return Err("this branch has no commit of its own to amend".to_string());
                }
                Ok(())
            }
            CommitMenuAction::StashStaged => {
                if staged.is_none() {
                    return Err("nothing staged to stash".to_string());
                }
                if !has_message {
                    return Err("write a message first".to_string());
                }
                Ok(())
            }
        }
    }

    /// The composer's target branch, or `None` for a detached `HEAD` - the one lookup both the
    /// composer's own right-aligned label and the `Commit and push` row read.
    pub(in crate::sidebar) fn composer_branch(&self) -> Option<String> {
        self.worktrees
            .iter()
            .find(|item| item.path == self.diff_root)
            .and_then(|item| item.branch.clone())
    }

    /// Runs one `▾` menu action for real, on the background executor, under the same single-flight
    /// guard and the same status-line reporting `Self::commit_staged_files` already uses.
    pub(in crate::sidebar) fn run_commit_menu_action(
        &mut self,
        action: CommitMenuAction,
        cx: &mut Context<Self>,
    ) {
        if self.commit_menu_availability(action).is_err() {
            return;
        }
        self.commit_menu_open = false;

        let worktree_path = self.diff_root.clone();
        let branch_display = self.branch_display_for(&worktree_path);
        let branch = self.composer_branch();
        let message = self.staged_commit_message();
        let staged = self.staged_uncommitted_paths().unwrap_or_default();
        let file_count = staged.len();

        self.worktree_history_op_in_flight = Some(worktree_history::WorktreeHistoryOpKind::Commit);
        self.worktree_history_status = Some(match action {
            CommitMenuAction::CommitAndPush => format!(
                "committing and pushing {} in {branch_display}\u{2026}",
                plural::count(file_count, "file", None)
            ),
            CommitMenuAction::CommitAllFiles => {
                format!("committing everything in {branch_display}\u{2026}")
            }
            CommitMenuAction::AmendLastCommit => format!(
                "amending {branch_display}'s last commit with {}\u{2026}",
                plural::count(file_count, "file", None)
            ),
            CommitMenuAction::StashStaged => format!(
                "stashing {} in {branch_display}\u{2026}",
                plural::count(file_count, "file", None)
            ),
        });
        cx.notify();

        let run_path = worktree_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result: Result<String, String> = cx
                .background_executor()
                .spawn(async move {
                    match action {
                        CommitMenuAction::CommitAndPush => {
                            wt_core::undo::commit_paths(&run_path, &staged, &message)
                                .map_err(|err| err.to_string())?;
                            // Pushed by name, not by a bare `git push`: `push_branch` sets an
                            // upstream when the branch has none, which is the normal state for a
                            // worktree branch Jerry itself created.
                            let branch = branch.ok_or_else(|| {
                                "HEAD is detached, so there is no branch to push".to_string()
                            })?;
                            wt_core::remote::push_branch(
                                &run_path,
                                &branch,
                                wt_core::remote::PushForce::None,
                            )
                            .map_err(|err| err.to_string())?;
                            Ok(format!(
                                "committed {} and pushed {branch}",
                                plural::count(file_count, "file", None)
                            ))
                        }
                        CommitMenuAction::CommitAllFiles => {
                            wt_core::undo::commit_all_changes(&run_path, &message)
                                .map_err(|err| err.to_string())?;
                            Ok("committed every uncommitted file".to_string())
                        }
                        CommitMenuAction::AmendLastCommit => {
                            wt_core::undo::amend_head_with_paths(&run_path, &staged)
                                .map_err(|err| err.to_string())?;
                            Ok(format!(
                                "amended the last commit with {}",
                                plural::count(file_count, "file", None)
                            ))
                        }
                        CommitMenuAction::StashStaged => {
                            wt_core::undo::stash_staged(&run_path, &format!("jerry: {message}"))
                                .map_err(|err| err.to_string())?;
                            Ok(format!(
                                "stashed {}",
                                plural::count(file_count, "file", None)
                            ))
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.worktree_history_op_in_flight = None;
                match result {
                    Ok(status) => {
                        this.worktree_history_status =
                            Some(format!("{status} in {branch_display}"));
                        // Every one of these four really moves the index, so the app's own idea of
                        // what is staged is re-derived from real git by the reload below rather
                        // than patched here - see `Self::load_diff`'s own docs on why staged state
                        // is re-queried instead of cached.
                        this.load_diff(worktree_path.clone(), cx);
                    }
                    Err(err) => {
                        this.worktree_history_status =
                            Some(format!("{} failed: {err}", action.label()));
                    }
                }
                cx.notify();
            });
        });
        self._worktree_history_task = Some(task);
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
        let visible_indices = self.file_tree.visible_indices(&self.expanded_dirs);
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
        // a "... and N more" row was the dishonesty issue #18 §4 set out to remove. The
        // *load*-time cap that used to survive alongside it (`Settings.file_tree.max_entries`,
        // plus its "load more" action) is gone too, as of GitHub issue #160 - there is no cap of
        // any kind on this tree now.
        let list = uniform_list(
            "file-tree-list",
            rendered_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, window, cx| {
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
                            .map(|entry| this.render_file_tree_row(&entry, &marks, window, cx)),
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
            .children(scrollbar::render_vertical_scrollbar(
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
        // No truncation row and no "load more" action live here any more: GitHub issue #160
        // removed the walk's entry cap, so there is no "stopped early" state left to disclose.
        // The tree below `list` is the whole tree.

        // The real, honest surface for a failed file operation (a refused rename, a trash
        // command that didn't run) - next to the tree it happened in, not buried in the log.
        if let Some(error) = self.tree_op_error.clone() {
            // GitHub issue #128.
            let row = hover_bg(
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
                    .cursor_pointer(),
                theme::surface::ROW_HOVER,
            );
            column = column.child(
                row.tooltip(text_tooltip("Click to dismiss"))
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
    fn file_tree_shell(&self, body: gpui::AnyElement, cx: &mut Context<Self>) -> gpui::AnyElement {
        // Space-separated context *words*, which is what `KeyBindingContextPredicate`'s
        // identifier terms match against - so `Some("file-tree && !tree-editing")` really does
        // stop matching the moment `"tree-editing"` is added. An open inline name editor adds
        // that word, plus `"text-input"` on top - GitHub issue #17's one shared tag for every
        // real text-typing surface, routing `Ctrl+Z` in a rename box to `TextUndo` (not
        // `FileTreeUndo` - see `crate::sidebar::tree_ops::AdeApp::handle_tree_text_undo`, the
        // listener the tag routes to instead) rather than the file tree's own undo/redo (GitHub
        // issue #105).
        let key_context =
            crate::keymap_overrides::file_tree_key_context(self.tree_inline_edit.is_some());
        let shell = div()
            .id("file-tree-shell")
            .key_context(key_context)
            .track_focus(&self.tree_focus_handle)
            .on_action(cx.listener(Self::handle_file_tree_rename_action))
            .on_action(cx.listener(Self::handle_file_tree_copy_action))
            .on_action(cx.listener(Self::handle_file_tree_cut_action))
            .on_action(cx.listener(Self::handle_file_tree_paste_action))
            .on_action(cx.listener(Self::handle_file_tree_delete_action))
            .on_action(cx.listener(Self::handle_file_tree_undo_action))
            .on_action(cx.listener(Self::handle_file_tree_redo_action))
            .on_action(cx.listener(Self::handle_tree_text_undo))
            .on_action(cx.listener(Self::handle_tree_text_redo))
            .on_key_down(cx.listener(Self::handle_tree_key_down));
        // GitHub issue #336's four clipboard/select-all actions, on the same node the
        // `"text-input"` word above lives on. They are only ever *reachable* while an inline name
        // editor is open, since that word only appears then - and `tree_inline_edit_handle`
        // independently answers `None` when it is not, so a stray dispatch is a real no-op rather
        // than an edit to a field that isn't there.
        let shell = self
            .wire_text_input_actions(shell, tree_inline_edit_handle(), cx)
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
            // GitHub issue #148: the empty area is also a real drop target - "move to the
            // worktree root", the same `destination_dir` every other empty-area action
            // (New File/New Folder/Paste) already resolves to. Any row's own `on_drop` calls
            // `cx.stop_propagation()`, so this only ever fires for a drop that genuinely misses
            // every row.
            .on_drop(
                cx.listener(move |this, dragged: &TreeDragPayload, _window, cx| {
                    let root = this.file_tree_root.clone();
                    this.move_paths_into_dir(&dragged.paths, &root, cx);
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
            .child(body);
        shell.into_any_element()
    }

    /// The rows [`Self::render_file_tree`]'s `uniform_list` will build, and the indent depth the
    /// inline editor row (if any) should be drawn at.
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
    fn render_tree_inline_edit_row(
        &self,
        depth: usize,
        cx: &mut Context<Self>,
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
            // GitHub issue #123: real clearance from the file tree's own overlay scrollbar
            // (`crate::root::scrollbar::CONTENT_CLEARANCE`'s own docs), not the bare
            // `SCROLLBAR_SIZE` this used to match exactly (flush contact, not a gap). Matches
            // `Self::render_file_tree_row`'s own row padding below so the inline editor stays
            // visually aligned with the normal rows around it.
            .pr(px(scrollbar::CONTENT_CLEARANCE))
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
                // GitHub issue #336: a real caret *bar* at the real insertion point, through the
                // one helper that owns that structure - not the `\u{2502}` glyph appended to the
                // text this row used to draw, which could only ever sit at the end of the name
                // however far back the user had arrowed, and which no selection highlight or
                // click hit-testing could be built on.
                self.render_simple_input_row(
                    SimpleInput {
                        caret_selector: "file-tree-inline-edit-caret".into(),
                        text_selector: "file-tree-inline-edit-text".into(),
                        focus_handle: Some(&self.tree_focus_handle),
                        text: &name,
                        caret_offset: edit.name.caret(),
                        selection: edit.name.selection(),
                        placeholder: "",
                        font: theme::font::MONO,
                        text_size: self.ui_text_size(11.5),
                        text_color: theme::text::STRONG,
                        placeholder_color: theme::text::GHOST,
                        caret: SimpleInputCaret::default(),
                        field: Some(tree_inline_edit_handle()),
                    },
                    cx,
                ),
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
        window: &Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let indent = px(file_tree::INDENT_STEP * entry.depth as f32);
        let is_open = entry.is_dir && self.expanded_dirs.contains(&entry.path);
        let mark = marks.get(&entry.path).copied();
        // GitHub issue #127: the row-selection highlight (README's Zone 3 "Selected row bg
        // `#1a1e21`") used to key purely off `Self::selected_tree_path` - the last-opened path,
        // set by `Self::open_file_view`/`Self::open_palette_file_result` - with no check against
        // real keyboard focus at all. That left the row looking selected/focused indefinitely
        // after focus genuinely moved to the editor, a terminal, or the Changes panel. (The
        // indent guides just below this used to have their own ancestor-chain highlight built on
        // the same idea; GitHub issue #406 removed that entirely - see the guides' own comment.)
        //
        // Two genuinely different things were being conflated under one field, though: "this row
        // is what the centre pane is actually showing" (should stay lit no matter where focus
        // is, the same way VS Code keeps the open file highlighted in its explorer while you're
        // typing in the editor) versus "this row is the tree's own keyboard-navigation cursor"
        // (should go idle the moment focus genuinely leaves the tree - a stale cursor left glowing
        // over whatever directory you last clicked is exactly the bug this issue was filed for).
        // `Self::open_change_absolute_path` answers the first question directly from
        // `Self::open_change` - independent of `selected_tree_path`, which a directory click (or
        // a tab-strip switch, which never touches it at all) can leave pointing somewhere the
        // centre pane isn't showing.
        let tree_focused = self.tree_focus_handle.is_focused(window);
        let open_change_path = self.open_change_absolute_path();
        let is_open_file = open_change_path.as_deref() == Some(entry.path.as_path());
        // GitHub issue #145: a Ctrl/Cmd- or Shift-selected row (beyond the anchor) highlights
        // exactly like the anchor itself does - same focus gating, since it's a keyboard-
        // navigation-style selection, not a standing "this is open" marker the way `is_open_file`
        // is.
        let is_selected = is_open_file
            || (tree_focused
                && (self.selected_tree_path.as_deref() == Some(entry.path.as_path())
                    || self.additional_tree_selection.contains(&entry.path)));

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
            // GitHub issue #123 ("Add padding to the file tree right side icons/buttons"): this
            // row's own trailing "new file" `+` control (added below, when `entry.is_dir`) used
            // to sit exactly `SCROLLBAR_SIZE` from the row's right edge - precisely where the
            // real overlay scrollbar's track begins, i.e. flush contact rather than a real gap,
            // whenever the tree is tall enough to scroll. See
            // `crate::root::scrollbar::CONTENT_CLEARANCE`'s own docs for the reasoning behind the
            // exact value.
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(11.5))
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED))
            // GitHub issue #148: the drop-target folder while a real file-tree drag is over it -
            // `theme::status::ASK_BG`, the same "you can act here" amber this app already uses
            // for its other real actionable notices, not a new one-off token. Deliberately
            // overrides `is_selected`'s own bg (a `.when` after it, not combined into one
            // condition) - a drop target reads as "drop here", not "also selected", even for the
            // rare case of dragging onto an already-selected folder.
            .when(
                entry.is_dir
                    && self.tree_drag_hover_target.as_deref() == Some(entry.path.as_path()),
                |el| el.bg(theme::status::ASK_BG),
            );

        // Indent guides (issue #18 §3), one per level of nesting between this row and the root.
        //
        // Drawn as this row's *own* absolutely-positioned children rather than as one overlay
        // across the list, which is what makes them correct under `uniform_list`'s
        // virtualization for free: a guide is a pure function of the row it belongs to
        // (just `entry.depth`), so a recycled row can only ever draw the guides that genuinely
        // belong to whatever row it now shows. An overlay would instead have to track the
        // visible range and scroll offset itself and stay in step with them - the real source of
        // the "gaps or misaligned segments as rows recycle" failure the issue calls out. Each
        // guide spans the row's full 22px height with no gap or inset, so consecutive rows'
        // segments meet exactly and read as one continuous line down the subtree.
        //
        // Always the resting `theme::tree::INDENT_GUIDE` colour, regardless of selection, focus,
        // or which file is open (GitHub issue #406) - a guide is pure tree structure, not a
        // "this leads to something" marker. This used to also highlight the open file's own
        // ancestor chain in an accent colour (`theme::tree::INDENT_GUIDE_ACTIVE`, added for
        // GitHub issue #127), via `file_tree::active_guide_levels`. That branch is deliberately
        // gone rather than disabled: reported as a visual bug against a clear product
        // preference ("don't color the lines in the file explorer, let them neutral color like
        // not selected"), not a case of the feature merely being wrong under some states - so
        // there's nothing left here that can recolour a guide.
        for level in 0..entry.depth {
            row = row.child(
                div()
                    // Test-only (a no-op outside test builds, like the row's own selector
                    // above): the only way a real render test can prove a guide painted at the
                    // right x, at the right height, on the right row - including after
                    // `uniform_list` has recycled that row's element. Keyed on `entry.name` like
                    // the row selector above, so two same-named files in different folders would
                    // collide - every test using these gives its fixtures unique names.
                    .debug_selector(|| format!("file-tree-guide-{}-{level}", entry.name))
                    .absolute()
                    .top_0()
                    .h_full()
                    .w(px(1.0))
                    .left(px(file_tree::indent_guide_x(level)))
                    .bg(theme::tree::INDENT_GUIDE),
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

        // GitHub issue #148: every row can be dragged - a row already part of a real
        // multi-selection drags the *whole* selection, matching how a click on it wouldn't
        // collapse the selection either (a plain click on an *unselected* row still resets the
        // selection first, the same way `Self::tree_click_select`'s own plain-click branch
        // does, so the drag that follows only ever carries what's genuinely selected at drag
        // start). Only real per-row state is read here - never anything that could go stale by
        // the time the drag actually starts, since `drag_value` is captured once, now.
        {
            let is_selected = self.is_tree_path_selected(&entry.path);
            let drag_paths = if is_selected && self.tree_selection_len() > 1 {
                self.tree_selected_paths()
            } else {
                vec![entry.path.clone()]
            };
            let label = if drag_paths.len() > 1 {
                plural::count(drag_paths.len(), "item", None)
            } else {
                entry.name.clone()
            };
            let drag_value = TreeDragPayload {
                paths: drag_paths,
                label,
            };
            row = row.on_drag(drag_value, move |dragged, _position, _window, cx| {
                cx.new(|_| dragged.clone())
            });
        }
        if entry.is_dir {
            #[allow(clippy::expect_used)]
            let drop_path = AdeApp::tree_drop_target_dir(&entry.path, true)
                .expect("a directory's own path is always its own drop target");
            let hover_path = entry.path.clone();
            row = row
                .on_drag_move(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<TreeDragPayload>, _window, cx| {
                        // A folder can't be its own drop target's *visual* indication either -
                        // `Self::move_paths_into_dir` already refuses the move itself, but
                        // highlighting a folder that's part of what's being dragged onto it
                        // would show an affordance for a drop this app is about to reject anyway.
                        let hovering = event.bounds.contains(&event.event.position)
                            && !event.drag(cx).paths.contains(&hover_path);
                        // Only touches state (and repaints) on a real change - matching
                        // `Self::update_tab_drag_insertion`'s own identical discipline, since
                        // `on_drag_move` fires on every real mouse-move during the drag.
                        if hovering {
                            if this.tree_drag_hover_target.as_deref() != Some(hover_path.as_path())
                            {
                                this.tree_drag_hover_target = Some(hover_path.clone());
                                cx.notify();
                            }
                        } else if this.tree_drag_hover_target.as_deref()
                            == Some(hover_path.as_path())
                        {
                            this.tree_drag_hover_target = None;
                            cx.notify();
                        }
                    },
                ))
                .on_drop(
                    cx.listener(move |this, dragged: &TreeDragPayload, _window, cx| {
                        // Without this, the drop would *also* reach `Self::file_tree_shell`'s own
                        // root-drop handler right after this row's - moving the same selection into
                        // the folder it was just dropped on, then straight back out to the root.
                        cx.stop_propagation();
                        this.tree_drag_hover_target = None;
                        this.move_paths_into_dir(&dragged.paths, &drop_path, cx);
                    }),
                );
        } else if let Some(drop_path) = AdeApp::tree_drop_target_dir(&entry.path, false) {
            // GitHub issue #152: a file row is not itself a drop target the way a folder row is
            // (there's nowhere "inside" a file to move something into), but it must still catch a
            // drop rather than let one fall through to `Self::file_tree_shell`'s own root-only
            // fallback below - most rows in any populated folder are files, not that folder's own
            // header row, so "release roughly where the dragged item already was" overwhelmingly
            // means releasing over a sibling *file*, not the parent directory's row. Without this,
            // that ordinary "changed my mind, drop it back" gesture silently relocated the whole
            // selection to the worktree root instead of leaving it alone - the real bug this
            // fixes. `Self::tree_drop_target_dir` resolves this to the file's own parent
            // directory - see that function's own docs, including why it's a real, directly
            // testable function rather than logic inlined only here.
            let hover_path = drop_path.clone();
            row = row
                .on_drag_move(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<TreeDragPayload>, _window, cx| {
                        // Highlights the *parent folder's* row (if visible), not this file row
                        // itself - the same "which row does dropping here really target"
                        // affordance the folder-row handler above gives, just one level up, since
                        // dropping is never "onto" a file the way it can be "into" a folder.
                        let hovering = event.bounds.contains(&event.event.position);
                        if hovering {
                            if this.tree_drag_hover_target.as_deref() != Some(hover_path.as_path())
                            {
                                this.tree_drag_hover_target = Some(hover_path.clone());
                                cx.notify();
                            }
                        } else if this.tree_drag_hover_target.as_deref()
                            == Some(hover_path.as_path())
                        {
                            this.tree_drag_hover_target = None;
                            cx.notify();
                        }
                    },
                ))
                .on_drop(
                    cx.listener(move |this, dragged: &TreeDragPayload, _window, cx| {
                        cx.stop_propagation();
                        this.tree_drag_hover_target = None;
                        this.move_paths_into_dir(&dragged.paths, &drop_path, cx);
                    }),
                );
        }

        if entry.is_dir {
            let path = entry.path.clone();
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    // Selecting *and* focusing, both real: a folder click is what gives the tree
                    // keyboard focus (so its `Ctrl+C`/`F2`/`Shift+F10` bindings can match at
                    // all) and what gives those bindings a target. Deliberately here, in the
                    // click handler, and not inside `toggle_dir_expanded` - that method is also
                    // called programmatically (`start_tree_new_entry`, the reveal paths), where
                    // moving the selection would be a side effect nobody asked for.
                    this.focus_file_tree(window, cx);
                    let modifiers = event.modifiers();
                    // GitHub issue #145: Ctrl/Cmd- or Shift-click only ever adjusts the
                    // selection, never the expand/collapse state - a modifier-click that also
                    // toggled the folder open would be a second, unrelated effect nobody asked
                    // for from what's meant to be a pure selection gesture.
                    if modifiers.secondary() || modifiers.shift {
                        this.tree_click_select(path.clone(), modifiers);
                    } else {
                        this.selected_tree_path = Some(path.clone());
                        this.additional_tree_selection.clear();
                        this.toggle_dir_expanded(path.clone(), cx);
                    }
                    cx.notify();
                }));
        } else {
            // GitHub issue #105: uses `Self::open_file_view_from_tree_click`, not
            // `Self::open_file_view` - a file click is a real *selection* gesture, the same as a
            // folder click just above (which has always kept tree focus), so this must never move
            // keyboard focus onto the editor the way every other `open_file_view` caller wants.
            // See that function's own docs for the real bug this fixes: every
            // `"file-tree"`-scoped shortcut (`Ctrl+C`/`X`/`V`, `F2`, `Shift+F10`) went dead the
            // instant a file was clicked, since that used to be the one tree gesture that silently
            // handed focus away.
            let path = entry.path.clone();
            row = row
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    this.focus_file_tree(window, cx);
                    let modifiers = event.modifiers();
                    // GitHub issue #145: a modifier-click only ever adjusts the selection, never
                    // opens the file - opening every Ctrl/Cmd- or Shift-selected file at once
                    // would be a real, surprising side effect of what's meant to be a pure
                    // selection gesture (and would fight the very "build up a selection" the
                    // modifier is for).
                    if modifiers.secondary() || modifiers.shift {
                        this.tree_click_select(path.clone(), modifiers);
                        cx.notify();
                    } else {
                        this.open_file_view_from_tree_click(path.clone(), window, cx);
                    }
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
                    .truncate()
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
                    // Test-only (see `crate::root::scrollbar`'s own `render_vertical_scrollbar`
                    // for the identical pattern) - `.id()` alone doesn't register a
                    // `debug_bounds`-queryable selector, so GitHub issue #123's own geometric
                    // regression test needs this to read this button's real painted bounds.
                    .debug_selector({
                        let path = entry.path.clone();
                        move || format!("file-tree-new-file-{}", path.display())
                    })
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

    /// Zone 3's header band (36 high): the real `Files · Search · Changes` segmented control
    /// (`design_handoff_jerry_ade/README.md`: "Header 36: segmented `Files | Changes`
    /// (Files is first and default...)", with GitHub issue #162 adding Search in the middle) plus
    /// the real `+n`/`−n` totals across the currently loaded diff, summed from the same real
    /// per-file stats (`crate::sidebar::changes::diff_file_stats`) the Changes rows themselves
    /// show.
    pub(in crate::sidebar) fn render_right_sidebar_toggle(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = match self.right_sidebar_view {
            RightSidebarView::Files => "Files",
            RightSidebarView::Search => "Search",
            RightSidebarView::Changes => "Changes",
        };
        let toggle = self.render_choice_control(
            "right-sidebar-toggle",
            &[
                ChoiceOption::with_icon(
                    "Files",
                    crate::icons::Icon::Folder,
                    "Files \u{2014} the file tree for this worktree",
                ),
                ChoiceOption::with_icon(
                    "Search",
                    crate::icons::Icon::MagnifyingGlass,
                    "Search \u{2014} find in this worktree",
                ),
                ChoiceOption::with_icon(
                    "Changes",
                    crate::icons::Icon::GitBranch,
                    "Changes \u{2014} runs, uncommitted, commits and the branch diff",
                ),
            ],
            selected.to_string(),
            cx,
            |this, index, window, cx| {
                // Structural, not a label re-match: index 0 is `Files`, 1 is `Search`, 2 is
                // `Changes`, per the `options` array literal right above - see
                // `Self::render_choice_control`'s own docs for why dispatch is index-based.
                let view = match index {
                    1 => RightSidebarView::Search,
                    2 => RightSidebarView::Changes,
                    _ => RightSidebarView::Files,
                };
                this.set_right_sidebar_view(view, window, cx);
            },
        );

        let totals = self.diff_totals;

        div()
            // See `crate::work_surface::render::AdeApp::render_tab_strip`'s own selector: the
            // three column headers are measured together, so all three need one.
            .debug_selector(|| "right-panel-header".to_string())
            .flex_none()
            .h(theme::band::CHROME_HEADER)
            .flex()
            .items_center()
            .pl(px(10.0))
            // GitHub issue #123 ("Add padding to the file tree right side icons/buttons"): this
            // header sits directly above the file tree's own overlay scrollbar track (same
            // `w_full` column, same right edge - `Self::render_right_sidebar`'s `container` has
            // no side padding of its own), so its right-aligned action cluster below (collapse-
            // all, "New file" `+`) needs the same real clearance the tree's own rows now use,
            // not the plain `px(10.0)` this used to share on both sides - that left only 2px of
            // real gap past the scrollbar's own `SCROLLBAR_SIZE`, i.e. barely more than flush,
            // which is what the issue's screenshot shows. See
            // `crate::root::scrollbar::CONTENT_CLEARANCE`'s own docs for the value.
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            // The third of the window's three column headers, so the rule it draws is the same
            // one the sidebar strip and the centre tab strip draw - GitHub issue #291 /
            // `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4v: "all three borders
            // are `#191c1f` ... [otherwise it] would have read as one rule changing shade
            // mid-span". This was `theme::border::INNER` (`#1c2023`), a third shade on the same y.
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(toggle)
            // Everything below is one right-aligned action cluster - a `flex_1` spacer, not
            // `justify_between`, keeps it pinned to the trailing edge instead of spreading
            // apart (and stranding the collapse-all caret alone in the middle) whenever fewer
            // than all four children are present.
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
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
                    // "Collapse all" (issue #18 §1) - resets the tree *and* this worktree's
                    // saved fold state in one step, so it genuinely undoes the expansions rather
                    // than hiding them until the next launch. Files view only, like the "+"
                    // beside it.
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
                                // The same "\u{25be} pointing down" glyph `render_tree_caret`
                                // uses for an open folder, since this is the action that closes
                                // every one of them.
                                .child("\u{25be}")
                                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                    this.collapse_all_dirs(cx);
                                })),
                        )
                    })
                    // Root-level "New file" - creates directly in the worktree root, the one
                    // location the per-directory "+" on `Self::render_file_tree_row` can't reach
                    // (the root itself has no row of its own to attach to). Only shown for the
                    // Files view - the Changes list has no directory concept to anchor a
                    // "new file" affordance to.
                    .when(self.right_sidebar_view == RightSidebarView::Files, |el| {
                        let root = self.file_tree_root.clone();
                        el.child(
                            div()
                                .id("file-tree-new-file-root")
                                // Test-only (see `crate::root::scrollbar`'s own
                                // `render_vertical_scrollbar` for the identical pattern) - lets a
                                // real render test read this cluster's actual right-most painted
                                // bounds back, rather than only proving a padding *number*
                                // changed.
                                .debug_selector(|| "file-tree-new-file-root".to_string())
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
                                .on_click(cx.listener(
                                    move |this, _event: &ClickEvent, window, cx| {
                                        this.start_new_file(root.clone(), window, cx);
                                    },
                                )),
                        )
                    }),
            )
    }

    /// Zone 3's whole real body: the `Files | Changes` header, then either the scrollable file
    /// tree, or the Changes list's own header/scrollable-rows/footer trio -
    /// `design_handoff_jerry_ade/README.md`'s Changes spec ("Header 7/12 ... Footer 29"). Both
    /// list arms wrap their list in a plain `flex_1().min_h_0()` column, so a long list scrolls
    /// under its own pinned header/footer instead of pushing them off-screen.
    pub(crate) fn render_right_sidebar(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // The git graph tab (GitHub issue #1, phase (a)) replaces this whole panel with Commit/
        // Branches while it's focused - design spec §5 ("Files/Changes is replaced by Commit |
        // Branches"). `Self::right_sidebar_view` (Files/Changes) is left untouched underneath, so
        // switching back to an agent or file tab shows exactly what was there before.
        if self.graph_tab_active {
            return self.render_graph_right_panel(cx);
        }

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
                    self.tree_inline_edit.is_none(),
                )),
            // GitHub issue #162 / `REVISION-2026-08-14.md` §5: the query row (with the replace
            // and glob rows it reveals), the count row, then the body - which is the two-level
            // match tree, or one of the two message states, gated on one has-query flag. The whole
            // panel is `crate::search::render`'s; this arm only places it.
            RightSidebarView::Search => container.child(self.render_search_panel(cx)),
            // GitHub issue #285: four collapsible sections, with the commit composer pinned
            // **above** them rather than at the panel's foot. The composer is a git control and
            // the sections are what it acts on, so it reads first; that ordering is
            // `REVISION-2026-08-14.md` §1's own sketch. Of the four, only Runs is pinned to the
            // panel's own *bottom*, in its own capped well below the other three's shared
            // scroller (`Self::render_changes_runs_section`) - `Jerry.dc.html` line 1433's own
            // separate `max-height:170px` wrapper, not a fourth entry in the shared one.
            RightSidebarView::Changes => container
                .child(self.render_commit_composer(cx))
                .children(self.render_changes_row_error(cx))
                // Not `.overflow_y_scroll()` - `Self::render_changes_sections`' `gpui::list` owns
                // its own scrolling, exactly like the Files arm's `uniform_list` above.
                .child(
                    div()
                        .id("right-sidebar-body")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .child(self.render_changes_sections(cx)),
                )
                .child(self.render_changes_runs_section(cx))
                .child(render_changes_footer(
                    self.ui_text_size(10.0),
                    self.change_stage_toggle_live(),
                    self.change_seen_toggle_live(),
                    self.window_controls_style().is_macos(),
                    self.change_author_filter_live(),
                )),
        }
        .into_any_element()
    }

    /// The real base branch the panel's Commits and Against-main sections are scoped to, or `None`
    /// when git could not detect one (this worktree *is* the default branch, no default branch
    /// exists, or the histories are unrelated - see `wt_core::diff::DiffBase::NoBase`).
    pub(in crate::sidebar) fn changes_base_branch(&self) -> Option<&str> {
        self.branch_commits
            .loaded()
            .and_then(|commits| commits.base_branch.as_deref())
    }

    /// The Runs section's rows: one per real agent session open in this worktree, with its
    /// diffstat read straight off the uncommitted change set's own per-author partition.
    pub(in crate::sidebar) fn changes_run_rows(&self, cx: &gpui::App) -> Vec<sections::RunRow> {
        let sources: Vec<sections::RunSource> = self
            .current_worktree_agents()
            .filter_map(|agent| {
                let ProcessKind::Agent(kind) = agent.kind else {
                    return None;
                };
                let status = self.agent_status(agent, cx);
                // "Live" is the process still being alive, not the app's urgency vocabulary: an
                // agent waiting on a question is still holding its worktree open and can still
                // write the moment it is answered, so its diff is no more final than a busy one's.
                let live = agent.pane.read(cx).is_running();
                Some(sections::RunSource {
                    agent_id: agent.id,
                    agent_key: crate::provenance::AgentKey::new(
                        crate::review::state::baseline_key(&agent.cwd, kind, agent.spawned_at_unix),
                    ),
                    agent_label: kind.label().to_string(),
                    initial: work_surface::agent_initial(agent.kind),
                    tint: work_surface::agent_tint(agent.kind),
                    live,
                    // A live run's age is how long it has been going; an ended one's is how long
                    // since it last did anything, which is when it ended. `Status` is consulted
                    // only to keep the two apart, never to fabricate a third answer.
                    elapsed: match (live, status) {
                        (true, _) => agent.spawned_at.elapsed(),
                        (false, _) => agent
                            .pane
                            .read(cx)
                            .idle_duration()
                            .unwrap_or_else(|| agent.spawned_at.elapsed()),
                    },
                })
            })
            .collect();
        sections::run_rows(&sources, &self.uncommitted_change_set)
    }

    /// The whole panel, flattened into one row list (GitHub issue #285) - split by
    /// [`sections::SectionRow::section`] into [`Self::render_changes_sections`]' shared scroller
    /// (Uncommitted, Commits, Against main) and [`Self::render_changes_runs_section`]'s own
    /// pinned-bottom well (Runs), rather than rendered as one flat list itself.
    pub(in crate::sidebar) fn changes_section_rows(
        &self,
        cx: &gpui::App,
    ) -> Vec<sections::SectionRow> {
        let base_branch = self.changes_base_branch().map(str::to_string);
        let mut rows: Vec<sections::SectionRow> = Vec::new();

        for section in sections::ChangesSection::ORDER {
            let body = self.changes_section_body(section, cx);
            // Against main is the one exception: it renders no row per file (see
            // `SectionRow::AgainstMainContext`'s own docs), so its count reads the real file
            // total directly rather than counting rows that do not exist.
            let count = match section {
                sections::ChangesSection::AgainstMain => self.change_set.len(),
                _ => body.iter().filter(|row| row.is_counted()).count(),
            };
            let open = self.changes_sections.is_open(section);
            rows.push(sections::SectionRow::Header(sections::SectionHeader {
                section,
                label: section.label(base_branch.as_deref()),
                count,
                stat: self.changes_section_stat(section, &body),
                open,
                seen: match section {
                    sections::ChangesSection::Uncommitted => Some((
                        self.seen_files
                            .seen_count(&self.diff_root, &self.uncommitted_change_set),
                        count,
                    )),
                    _ => None,
                },
            }));
            if open {
                rows.extend(body);
            }
        }
        rows
    }

    /// One section's body rows, in render order.
    fn changes_section_body(
        &self,
        section: sections::ChangesSection,
        cx: &gpui::App,
    ) -> Vec<sections::SectionRow> {
        use sections::{ChangesSection, NoteEmphasis, SectionRow};

        let quiet = |text: &str| SectionRow::Note {
            section,
            text: text.to_string(),
            emphasis: NoteEmphasis::Quiet,
        };
        let warn = |text: String| SectionRow::Note {
            section,
            text,
            emphasis: NoteEmphasis::Warning,
        };

        match section {
            ChangesSection::Runs => {
                let rows = self.changes_run_rows(cx);
                if rows.is_empty() {
                    return vec![quiet("no agents have run in this worktree yet")];
                }
                rows.into_iter().map(SectionRow::Run).collect()
            }
            ChangesSection::Uncommitted => {
                if let Some(error) = self.uncommitted_diff.error() {
                    return vec![warn(error.to_string())];
                }
                let Some(diff) = self.uncommitted_diff.loaded() else {
                    return vec![quiet("computing uncommitted changes...")];
                };
                // Rows come from the **change set**, not the diff's own file list: the change set
                // is keyed by path, so "a path appears once per worktree" (`REVISION-2026-08-14`
                // §1's rule 1) is a property of the list this renders rather than a rule this
                // loop has to remember. A diff that somehow named one path twice is already one
                // row by the time it gets here.
                let mut body: Vec<SectionRow> = (0..self.uncommitted_change_set.len())
                    .map(SectionRow::UncommittedFile)
                    .collect();
                if body.is_empty() {
                    return vec![quiet("nothing uncommitted in this checkout")];
                }
                if diff.truncated {
                    body.push(warn(
                        "diff truncated: this checkout's real changes exceeded wt_core::diff's \
                         own load limits, so some files or lines are missing from this list"
                            .to_string(),
                    ));
                }
                body
            }
            ChangesSection::Commits => {
                if let Some(error) = self.branch_commits.error() {
                    return vec![warn(error.to_string())];
                }
                let Some(commits) = self.branch_commits.loaded() else {
                    return vec![quiet("reading this branch's commits...")];
                };
                if commits.base_branch.is_none() {
                    // Honest, and distinct from "no commits": without a base there is no such
                    // thing as "the commits this branch added", so the section states that rather
                    // than showing an empty list that would read as "you have committed nothing".
                    return vec![quiet(
                        "no base branch to measure this branch's own commits against",
                    )];
                }
                if commits.commits.is_empty() {
                    return vec![quiet("nothing committed on this branch yet")];
                }
                let mut body: Vec<SectionRow> = commits
                    .commits
                    .iter()
                    .cloned()
                    .map(SectionRow::Commit)
                    .collect();
                if commits.truncated {
                    body.push(warn(
                        "this branch has more commits than wt_core::diff loads at once, so the \
                         list above is only its most recent"
                            .to_string(),
                    ));
                }
                body
            }
            ChangesSection::AgainstMain => {
                let Some(diff) = self.current_diff() else {
                    // The real reason, from the one place that words it - loading, a genuine
                    // `git` error, or an unborn `HEAD` with no base at all - never a blanket
                    // "computing..." that would keep claiming to be busy after a real failure.
                    let (text, color) = self.diff_state_message();
                    return vec![SectionRow::Note {
                        section,
                        text,
                        emphasis: if color == theme::status::FAIL {
                            NoteEmphasis::Warning
                        } else {
                            NoteEmphasis::Quiet
                        },
                    }];
                };
                if self.change_set.is_empty() {
                    return vec![quiet("this branch matches its base exactly")];
                }
                let mut body: Vec<SectionRow> = Vec::new();
                // The read-only context card, deliberately **not** a counted row - see
                // `SectionRow::is_counted`. This section carries no action buttons at all: a
                // product decision recorded on GitHub issue #285, overriding
                // `REVISION-2026-08-14.md` §1's rule 3 and `STAGE-A-CHANGELOG.md` §4e. Merging a
                // branch into its base is the git graph's job (issue #241) and removing a
                // worktree already has its own entry point on the rail's worktree context menu,
                // so putting either here would be a second home for an action that already has
                // one - the exact defect §4e removed them from the agent header for.
                //
                // No per-file rows, though issue #285's own checklist says "diffstat, file list
                // and commit context" - a deliberate deviation from that restated checklist back
                // toward `Jerry.dc.html` itself: line 1422's `baseRows` is a synthetic one-entry
                // array (`wtBaseDefs`'s `files` is a plain count, never an array of files), so a
                // committed file was never meant to be its own row here. Requested directly:
                // "commited files should not appear on the changes tab under against master."
                let base = self.changes_base_branch().map(str::to_string);
                if let Some(base) = base {
                    body.push(SectionRow::AgainstMainContext {
                        text: format!(
                            "{} would land on {base}",
                            plural::count(self.change_set.len(), "file", None)
                        ),
                        // Real `git rev-list --left-right --count` figures the app already
                        // refreshes for the status bar, never recounted here. With none cached
                        // yet, the card states the merge-base it *is* scoped to rather than
                        // printing `0 ahead \u{b7} 0 behind`, which would be a claim git has not
                        // made.
                        sub: match self.ahead_behind_cache.get(&self.diff_root) {
                            Some(counts) => {
                                format!("{} ahead \u{b7} {} behind", counts.ahead, counts.behind)
                            }
                            None => format!("merge-base {}", short_sha(&diff.base_commit)),
                        },
                    });
                }
                if diff.truncated {
                    body.push(warn(
                        "diff truncated: this branch's real changes exceeded wt_core::diff's own \
                         load limits, so some files or lines are missing from this list"
                            .to_string(),
                    ));
                }
                body
            }
        }
    }

    /// One section's header diffstat.
    fn changes_section_stat(
        &self,
        section: sections::ChangesSection,
        body: &[sections::SectionRow],
    ) -> crate::provenance::DiffStat {
        use crate::provenance::DiffStat;
        use sections::{ChangesSection, SectionRow};
        match section {
            ChangesSection::Runs => body
                .iter()
                .fold(DiffStat::default(), |total, row| match row {
                    SectionRow::Run(run) => total.plus(run.stat),
                    _ => total,
                }),
            ChangesSection::Uncommitted => self.uncommitted_change_set.total(),
            ChangesSection::Commits => self
                .branch_commits
                .loaded()
                .map(|commits| DiffStat::new(commits.added, commits.removed))
                .unwrap_or_default(),
            ChangesSection::AgainstMain => self.change_set.total(),
        }
    }

    /// Opens or closes one section. Per section, and touching no other one.
    pub(in crate::sidebar) fn toggle_changes_section(
        &mut self,
        section: sections::ChangesSection,
        cx: &mut Context<Self>,
    ) {
        self.changes_sections.toggle(section);
        cx.notify();
    }

    /// The Changes panel's main scroller: **three** of its four sections' worth of rows
    /// (Uncommitted, Commits, Against main) in a single `gpui::list`, with the same shared overlay
    /// scrollbar every other scrollable region in this app draws (`crate::root::scrollbar`). Runs
    /// is deliberately excluded - it renders in its own pinned-bottom well,
    /// [`Self::render_changes_runs_section`], matching `Jerry.dc.html` line 1433's own separate
    /// `max-height:170px;overflow-y:auto` wrapper rather than sharing this scroller.
    pub(in crate::sidebar) fn render_changes_sections(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let rows: std::rc::Rc<Vec<sections::SectionRow>> = std::rc::Rc::new(
            self.changes_section_rows(cx)
                .into_iter()
                .filter(|row| row.section() != sections::ChangesSection::Runs)
                .collect(),
        );
        // `ListState` owns a measured height per item, so it has to be told when the item set
        // changes size. Reset only on a real change: a reset drops the scroll position, and doing
        // it every frame would pin the panel to the top. `ListState::reset` takes `&self` (its
        // state is behind an `Rc<RefCell<_>>`), which is what lets this run from a `&self` render.
        if self.changes_sections_list.item_count() != rows.len() {
            self.changes_sections_list.reset(rows.len());
        }

        let build_rows = rows.clone();
        let list = gpui::list(
            self.changes_sections_list.clone(),
            cx.processor(
                move |this: &mut Self,
                      index: usize,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
                    // Bounds-checked rather than indexed: the row list is a snapshot taken when
                    // this frame's `render` ran, and a diff replaced between then and now must
                    // render nothing rather than panic - the same defensive re-resolve the file
                    // tree's own virtualized list documents.
                    match build_rows.get(index) {
                        Some(row) => this.render_section_row(row, window, cx),
                        None => div().into_any_element(),
                    }
                },
            ),
        )
        .w_full()
        .flex_1()
        .min_h_0();

        // See `Self::render_file_tree`'s own docs on why the scrollbar must be a sibling of the
        // list, inside its own non-scrolling `.relative()` wrapper.
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .children(scrollbar::render_vertical_scrollbar(
                "changes-sections-scrollbar",
                &self.changes_sections_list,
                &[],
                cx,
            ))
            .into_any_element()
    }

    /// The Runs section, pinned to the Changes panel's own bottom in its own capped,
    /// independently-scrolled well - `Jerry.dc.html` line 1433's `flex:none;max-height:170px;
    /// overflow-y:auto` wrapper, which sits *outside* the shared scroller the other three
    /// sections share rather than as a fourth entry inside it (the user: "run row should be
    /// pinned to the bottom and not to the top look at the design").
    pub(in crate::sidebar) fn render_changes_runs_section(
        &self,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use sections::{ChangesSection, SectionRow};
        let rows: Vec<SectionRow> = self
            .changes_section_rows(cx)
            .into_iter()
            .filter(|row| row.section() == ChangesSection::Runs)
            .collect();
        div()
            .id("changes-runs-section")
            .flex_none()
            .max_h(px(170.0))
            .overflow_y_scroll()
            .bg(theme::surface::RUNS_WELL)
            .children(rows.iter().map(|row| {
                match row {
                    SectionRow::Header(header) => {
                        self.render_section_header(header, cx).into_any_element()
                    }
                    SectionRow::Run(run) => self.render_run_row(run, cx).into_any_element(),
                    SectionRow::Note {
                        section,
                        text,
                        emphasis,
                    } => self
                        .render_section_note(*section, text, *emphasis)
                        .into_any_element(),
                    // Runs never produces any other `SectionRow` kind - see `changes_section_body`.
                    _ => div().into_any_element(),
                }
            }))
            .into_any_element()
    }

    /// Dispatches one flattened row to the renderer for its kind.
    fn render_section_row(
        &self,
        row: &sections::SectionRow,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        use sections::{ChangesSection, SectionRow};
        match row {
            SectionRow::Header(header) => self.render_section_header(header, cx).into_any_element(),
            SectionRow::Run(run) => self.render_run_row(run, cx).into_any_element(),
            SectionRow::UncommittedFile(index) => {
                match self.uncommitted_change_set.entries().get(*index) {
                    Some(entry) => self
                        .render_change_row(entry, ChangesSection::Uncommitted, cx)
                        .into_any_element(),
                    None => div().into_any_element(),
                }
            }
            SectionRow::Commit(commit) => self.render_commit_row(commit).into_any_element(),
            SectionRow::AgainstMainContext { text, sub } => self
                .render_against_main_context(text, sub)
                .into_any_element(),
            SectionRow::Note {
                section,
                text,
                emphasis,
            } => self
                .render_section_note(*section, text, *emphasis)
                .into_any_element(),
        }
    }

    /// One section header: caret, uppercase label, count, and a right-aligned split diffstat, in
    /// the rev-6 `theme::changes` tokens (`REVISION-2026-08-14.md` §1). Clicking anywhere on it
    /// opens or closes the section.
    fn render_section_header(
        &self,
        header: &sections::SectionHeader,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let section = header.section;
        let selector = format!(
            "changes-section-{}-{}-{}",
            section.key(),
            header.count,
            if header.open { "open" } else { "collapsed" }
        );
        let stat_selector = match sections::section_diffstat(header.stat) {
            Some((add, del)) => format!("changes-section-{}-stat-{add}-{del}", section.key()),
            None => format!("changes-section-{}-stat-empty", section.key()),
        };
        let stat = sections::section_diffstat(header.stat);
        let seen = header.seen.and_then(|(seen, total)| {
            sections::seen_label(seen, total).map(|label| (label, seen, total))
        });

        div()
            .id(gpui::SharedString::from(format!(
                "changes-section-{}",
                section.key()
            )))
            .debug_selector(move || selector)
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap(px(7.0))
            .h(theme::band::CHANGES_SECTION_HEADER)
            .pl(px(8.0))
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            // The shared caret glyph inherits its colour from whatever wraps it (see
            // `render_disclosure_caret`'s own docs on why it paints none of its own). Set here
            // rather than on the glyph: this row's other children - the label, the count, the
            // seen meter and the diffstat - all name their own token, so nothing else inherits it.
            .text_color(theme::changes::SECTION_CARET)
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.toggle_changes_section(section, cx);
            }))
            .child(render_disclosure_caret(
                header.open,
                self.ui_text_size(10.0),
            ))
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::changes::SECTION_LABEL)
                    .child(header.label.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::changes::SECTION_COUNT)
                    .child(header.count.to_string()),
            )
            .child(div().flex_1().min_w_0())
            .when_some(seen, |el, (label, seen, total)| {
                const METER_WIDTH: f32 = 34.0;
                let fraction = sections::seen_fraction(seen, total);
                let seen_selector = format!("changes-section-uncommitted-{seen}-of-{total}-seen");
                el.child(
                    div()
                        .debug_selector(move || seen_selector)
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(10.0))
                        .text_color(theme::text::GHOSTER)
                        .child(label),
                )
                .child(
                    div()
                        .relative()
                        .flex_none()
                        .w(px(METER_WIDTH))
                        .h(px(3.0))
                        .rounded(px(1.5))
                        .bg(theme::diff::STAT_EMPTY)
                        .child(
                            div()
                                .absolute()
                                .left(px(0.0))
                                .top(px(0.0))
                                .h(px(3.0))
                                .w(px(METER_WIDTH * fraction))
                                .rounded(px(1.5))
                                .bg(theme::diff::STAT_ADD),
                        ),
                )
            })
            .child(
                div()
                    .debug_selector(move || stat_selector)
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .when_some(stat, |el, (add, del)| {
                        el.child(
                            div()
                                .text_color(theme::changes::SECTION_STAT_ADD)
                                .child(add),
                        )
                        .child(
                            div()
                                .text_color(theme::changes::SECTION_STAT_DEL)
                                .child(del),
                        )
                    }),
            )
    }

    /// One Runs row (`STAGE-A-CHANGELOG.md` §4l): line 1 is the title alone at full width, line 2
    /// is `<agent> · ended 2m` on the left with `12 files +285 −119` pushed right. Left edge is
    /// the run's own agent tint, and there is **no checkbox, ever** - a run is not a stageable
    /// thing, and one agent's share of a file the other agent also wrote is not separately
    /// stageable at all (`REVISION-2026-08-14.md` §1's table and rule 1).
    fn render_run_row(&self, run: &sections::RunRow, cx: &mut Context<Self>) -> impl IntoElement {
        let agent_id = run.agent_id;
        let selector = format!("changes-run-{}", run.agent_id);
        let stat_selector = format!(
            "changes-run-{}-stat-+{}-{}{}",
            run.agent_id, run.stat.added, "\u{2212}", run.stat.removed
        );
        div()
            .id(gpui::SharedString::from(format!(
                "changes-run-row-{}",
                run.agent_id
            )))
            .debug_selector(move || selector)
            .flex_none()
            .w_full()
            .flex()
            .items_start()
            .gap(px(8.0))
            .h(theme::band::RUN_ROW)
            .pt(px(7.0))
            .pb(px(8.0))
            .pl(px(8.0))
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .border_l_2()
            .border_color(run.tint_fg)
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_agent(agent_id, window, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .mt(px(1.0))
                    .w(px(14.0))
                    .h(px(14.0))
                    .rounded(theme::radius::CHIP)
                    .bg(run.tint_bg)
                    .flex()
                    .items_center()
                    .justify_center()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(8.0))
                    .text_color(run.tint_fg)
                    .child(run.initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .overflow_hidden()
                            .truncate()
                            .font(font(theme::font::SANS))
                            .text_size(self.ui_text_size(11.5))
                            .text_color(theme::text::STRONG)
                            .child(run.title.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_shrink_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .truncate()
                                    .font(font(theme::font::MONO))
                                    .text_size(self.ui_text_size(10.0))
                                    .text_color(run.meta_color())
                                    .child(run.meta.clone()),
                            )
                            .child(div().flex_1().min_w_0())
                            .child(
                                div()
                                    .debug_selector(move || stat_selector)
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .font(font(theme::font::MONO))
                                    .text_size(self.ui_text_size(10.0))
                                    .child(
                                        div()
                                            .text_color(theme::text::PATH)
                                            .child(run.files_label.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme::diff::STAT_ADD)
                                            .child(format!("+{}", run.stat.added)),
                                    )
                                    .child(
                                        div()
                                            .text_color(theme::diff::STAT_DEL)
                                            .child(format!("\u{2212}{}", run.stat.removed)),
                                    ),
                            ),
                    ),
            )
    }

    /// One Commits row: short sha, subject, and the commit's own diffstat. No checkbox and no left
    /// edge - a commit is neutral scope (`REVISION-2026-08-14.md` §1's table), and there is
    /// nothing about an already-written commit to stage.
    fn render_commit_row(&self, commit: &wt_core::diff::BranchCommit) -> impl IntoElement {
        let selector = format!("changes-commit-{}", commit.short_id);
        div()
            .debug_selector(move || selector)
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(theme::band::CHANGE_ROW)
            .pl(px(10.0))
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::PATH)
                    .child(commit.short_id.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(11.5))
                    .text_color(theme::text::DIM)
                    .child(commit.subject.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::diff::STAT_ADD)
                    .child(format!("+{}", commit.added)),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::diff::STAT_DEL)
                    .child(format!("\u{2212}{}", commit.removed)),
            )
    }

    /// The Against-main section's read-only context card - what would land, and the branch's
    /// ahead/behind. **No action buttons**: see `Self::changes_section_body`'s own comment for the
    /// product decision that keeps merge and worktree deletion out of this panel.
    fn render_against_main_context(&self, text: &str, sub: &str) -> impl IntoElement {
        let selector = format!("changes-against-main-context-{text}");
        div()
            .debug_selector(move || selector)
            .flex_none()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .py(px(8.0))
            .pl(px(10.0))
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .border_l_2()
            .border_color(theme::changes::EDGE_AGAINST_MAIN)
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(11.5))
                    .text_color(theme::text::SECONDARY)
                    .child(text.to_string()),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(sub.to_string()),
            )
    }

    /// A section's own message row - empty, loading, failed or truncated.
    fn render_section_note(
        &self,
        section: sections::ChangesSection,
        text: &str,
        emphasis: sections::NoteEmphasis,
    ) -> impl IntoElement {
        let selector = format!("changes-section-{}-note", section.key());
        div()
            .debug_selector(move || selector)
            .flex_none()
            .w_full()
            .py(px(7.0))
            .pl(px(10.0))
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .font(font(theme::font::SANS))
            .text_size(self.ui_text_size(10.5))
            .text_color(emphasis.color())
            .child(text.to_string())
    }

    /// One file row - a staging checkbox (Uncommitted only), git's own `A`/`M`/`D` status letter,
    /// `dir`/`name`, `+n`/`−n`, and the five-segment stat bar. Clicking anywhere on the row other
    /// than the checkbox itself (see [`Self::render_staging_checkbox`]'s `stop_propagation`) opens
    /// the file's diff and marks it seen.
    pub(in crate::sidebar) fn render_change_row(
        &self,
        entry: &crate::provenance::change_set::ChangeSetEntry,
        section: sections::ChangesSection,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = entry.path.clone();
        let open_path = path.clone();
        let staged = self.staged_files.contains(&entry.path);
        let selected = self.open_change.as_deref() == Some(entry.path.as_path());
        let stat = entry.stat();
        let (add, del) = (stat.added, stat.removed);
        let (dir, name) = changes::split_dir_name(&entry.path);
        let letter = changes::status_letter(entry.status);
        let segments = changes::stat_bar_segments(add, del);
        let stageable = section.has_checkboxes();
        // Only meaningful in the Against-main scope, which is the one that lists work already
        // written down - see this method's own docs.
        let committed = !stageable
            && section == sections::ChangesSection::AgainstMain
            && changes::is_committed_clean(&entry.path, self.dirty_files.as_ref());
        // `theme::changes`' own docs: the filename owns "seen", the checkbox owns "staged" - one
        // fact per channel.
        let seen = self.seen_files.is_seen(&self.diff_root, &entry.path, stat) && stageable;
        let renamed = self
            .diff_for(section)
            .map(|diff| {
                diff.files
                    .iter()
                    .any(|file| file.path == entry.path && changes::is_real_rename(file))
            })
            .unwrap_or(false);
        let selector_prefix = if stageable {
            "change-row".to_string()
        } else {
            format!("{}-row", section.key())
        };
        let row_selector = format!("{selector_prefix}-{}", entry.path.display());
        let edge_selector = format!("{row_selector}-selection-edge");
        let dir_selector = format!("{selector_prefix}-dir-{}", entry.path.display());
        let letter_selector = format!("{row_selector}-status-{}", letter.glyph());
        let name_selector = format!(
            "{row_selector}-name-{}",
            if seen { "seen" } else { "unseen" }
        );
        let section_edge = section.edge_color();
        // §4i's floating bar acts on a *live, uncommitted* file: "open it in the editor" and
        // "throw this file's changes away" are both meaningless for an Against-main row, which
        // lists work already committed. Same gate as the checkbox, for the same reason.
        let hovered = stageable
            && (self.change_row_hover.as_deref() == Some(entry.path.as_path())
                || self.change_row_actions_hover.as_deref() == Some(entry.path.as_path()));
        let discard_armed = self.change_row_discard_armed.as_deref() == Some(entry.path.as_path());

        div()
            .id(gpui::SharedString::from(row_selector.clone()))
            .debug_selector({
                let row_selector = row_selector.clone();
                move || row_selector
            })
            .relative()
            .flex()
            .w_full()
            .items_center()
            .gap(px(6.0))
            .h(theme::band::CHANGE_ROW)
            .pl(px(9.0))
            // GitHub issue #123: reuses the same shared clearance the file tree's own rows use
            // (`crate::root::scrollbar::CONTENT_CLEARANCE`'s own docs) - this row sits next to
            // the identical overlay scrollbar (`Self::render_changes_sections`'
            // "changes-sections-scrollbar"), so it needs the same real gap, not just a
            // similar-looking bare number that happens to already clear the *old*,
            // insufficient value.
            .pr(px(scrollbar::CONTENT_CLEARANCE))
            .cursor_pointer()
            // No bottom border at all - this row used to carry a permanent `border_b_1()`
            // alongside a conditional `border_l_2()` inside the `when` below, and because GPUI's
            // `Style::border_color` is one shared value for every edge of a single element (not
            // per-edge - confirmed directly in `gpui`'s own `style.rs`), selecting a row silently
            // recoloured the bottom edge to the selection colour too - a real border appearing
            // along the bottom on selection, not an intended static separator. The old
            // `border_l_2()` also only reserved its 2px of space *while* selected, shifting every
            // row's content 2px right the instant it was clicked. See
            // `crate::graph_view::render::AdeApp::render_graph_row`'s identical fix for the full
            // reasoning - same bug, same shape, same fix: no bottom border, and a real, separate,
            // always-painted child for the left edge.
            .child(
                div()
                    .debug_selector(move || edge_selector)
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    // The section's own edge colour (`REVISION-2026-08-14.md` §1's table:
                    // uncommitted blue, branch-scope violet) is what this channel now carries, and
                    // selection is painted over it - so an unselected row in a scoped section
                    // still states which scope it is in, which is the whole point of the edge.
                    .bg(if selected {
                        theme::border::SELECTED_EDGE.into()
                    } else {
                        section_edge.unwrap_or(work_surface::TRANSPARENT)
                    }),
            )
            .when(selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.open_change_diff(open_path.clone(), window, cx);
            }))
            // §4i's hover reveal, in two halves - see `AdeApp::change_row_hover`'s own docs for
            // why the bar's overhang makes one field impossible.
            .when(stageable, |el| {
                let enter_path = entry.path.clone();
                el.on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    this.set_change_row_hover(&enter_path, *hovered, false, cx);
                }))
            })
            .when(stageable, |el| {
                el.child(self.render_staging_checkbox(path.clone(), staged, cx))
            })
            // §4j: git's own letter, in a fixed column **ahead of the directory**, so every
            // filename in the list starts on the same x. This is the slot the `new`/`del` word
            // pill used to occupy at the *other* end of the row, where it could not align
            // anything and was absent on most rows.
            .child(
                render_status_letter(
                    gpui::SharedString::from(letter_selector.clone()),
                    letter,
                    self.ui_text_size(10.0),
                )
                .debug_selector(move || letter_selector),
            )
            .when(!dir.is_empty(), |el| {
                el.child(
                    // GitHub issue #243: a deeply nested worktree can produce a `dir` long enough
                    // to push `name` and the stat counts clean off the row's right
                    // edge - this used to be `.flex_none()` with no cap or overflow handling at
                    // all. Capped rather than left in the shared flex-shrink pool with `name`
                    // below: the filename is what actually identifies the row, so it keeps first
                    // claim on whatever space is available, and the directory prefix truncates on
                    // its own budget instead of squeezing the name down to make room.
                    div()
                        .debug_selector(move || dir_selector)
                        .flex_none()
                        .max_w(px(120.0))
                        .overflow_hidden()
                        .truncate()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(10.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("{dir}/")),
                )
            })
            .child({
                let name_cell = div()
                    .id(gpui::SharedString::from(name_selector.clone()))
                    .debug_selector(move || name_selector)
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .font_weight(if seen {
                        gpui::FontWeight::NORMAL
                    } else {
                        gpui::FontWeight::MEDIUM
                    })
                    .text_size(self.ui_text_size(11.5))
                    // `STAGE-A-CHANGELOG.md` §4i: the filename itself carries **seen**, because
                    // "when a row already contains the thing a state is about, style that thing".
                    // Staged is the checkbox's job and only the checkbox's - do not reintroduce a
                    // colour for it here.
                    .text_color(if !stageable {
                        theme::text::DIM
                    } else if seen {
                        theme::changes::FILENAME_SEEN
                    } else {
                        theme::changes::FILENAME_UNSEEN
                    })
                    .child(name);
                // §4i: "The convention is stated in the name's own tooltip", and the tooltip
                // states the *real* rule rather than "read once" - a file you read and the agent
                // then edits again reverts to unseen, which `SeenFiles` implements by storing the
                // diffstat the file had when it was marked. Verbatim from the design.
                if stageable {
                    name_cell.tooltip(text_tooltip(if seen {
                        Self::SEEN_TOOLTIP
                    } else {
                        Self::UNSEEN_TOOLTIP
                    }))
                } else {
                    name_cell
                }
            })
            // GitHub issue #287: who wrote this file, right after its name -
            // `REVISION-2026-08-14.md` §1's "agent chip per row […] amber ring when it has two
            // authors". `render_author_chips` owns the ring, the tooltips and both click
            // gestures; this row only says where the group goes and when it exists at all.
            //
            // `stageable` is the Uncommitted scope, which is exactly where the design puts these:
            // *"Each **Uncommitted** row gains a chip group after the filename"*. An Against-main
            // row lists work already committed, where "who wrote this line" is git's own question
            // and git's own answer (`git blame`), not this app's local, uncommitted-only record -
            // and the ring's filter would open a diff against a base these chips were never
            // measured over.
            //
            // The multi-agent gate (`REVISION-2026-07-31.md` §4) is repeated here rather than left
            // to `render_author_chips` alone, so a single-agent worktree does not build one
            // throwaway author `Vec` per row per frame for a group that is then never rendered.
            .when(stageable && self.worktree_has_multiple_agents(), |el| {
                el.children(self.render_author_chips(
                    "change-row",
                    &entry.path,
                    &crate::provenance::render::chip_authors(entry),
                    entry.is_shared(),
                    cx,
                ))
            })
            // Alongside the status letter, never instead of it: a file added by a commit on this
            // branch is genuinely both an `A` (relative to the base) and `committed`.
            .when(committed, |el| el.child(render_committed_tag()))
            // A rename gets a plain `M` from `changes::status_letter` (§4j's table has three
            // letters, not four), so this chip is what states the rename - see that function's
            // own docs.
            .when(renamed, |el| el.child(render_moved_tag()))
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
            .children(
                hovered.then(|| self.render_change_row_actions(&entry.path, discard_armed, cx)),
            )
    }

    /// §4i's floating hover bar: two icons, straddling the row's top edge, on the app's one
    /// popover chrome.
    fn render_change_row_actions(
        &self,
        path: &Path,
        discard_armed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let hover_path = path.to_path_buf();
        let editor_path = path.to_path_buf();
        let arm_path = path.to_path_buf();
        let discard_path = path.to_path_buf();
        let bar_selector = format!("change-row-actions-{}", path.display());
        let editor_selector = format!("change-row-open-in-editor-{}", path.display());
        let discard_selector = if discard_armed {
            format!("change-row-discard-confirm-{}", path.display())
        } else {
            format!("change-row-discard-{}", path.display())
        };

        // One shared 22x22 optical box for both icons - `REVISION-2026-08-14.md` §7 rule 7 ("a
        // row of icons needs one shared optical box, not one size per icon"). Only the hover
        // pair differs: neutral for `open`, red for `discard`, since the second one destroys
        // work.
        let icon_button = |id: String,
                           selector: String,
                           hover_bg: theme::ColorToken,
                           hover_fg: theme::ColorToken| {
            div()
                .id(gpui::SharedString::from(id))
                .debug_selector(move || selector)
                .flex_none()
                .w(px(22.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme::radius::CARD_SM)
                .cursor_pointer()
                .font(font(theme::font::MONO))
                .text_size(px(11.0))
                .text_color(theme::text::DIM)
                .hover(move |el| el.bg(hover_bg).text_color(hover_fg))
        };

        let bar = menu_popover_chrome(
            div()
                .id(gpui::SharedString::from(bar_selector.clone()))
                .debug_selector(move || bar_selector)
                .occlude()
                .absolute()
                // §4i right-aligns the bar (`right:7px` in the mock), but the mock's list has no
                // overlay scrollbar to clear and this panel's does - so this takes the same
                // shared clearance every other right-aligned thing in this scroller already
                // takes (`scrollbar::CONTENT_CLEARANCE`'s own docs: "the one shared constant
                // everywhere right-aligned content sits next to this scrollbar, instead of every
                // call site repeating its own guess"). A literal 7 would put the discard button
                // under the track, which is the exact collision GitHub issue #123 fixed once.
                .right(px(scrollbar::CONTENT_CLEARANCE))
                // Straddling, not sitting on: the bar is 30px tall (22px buttons + 3px padding
                // + 1px border each side) and hangs 11px above the row's own top edge, which is
                // what makes it read as floating over the list instead of as one more thing
                // inside the row (§4i's first cut sat flush inside and was rejected for exactly
                // that).
                .top(px(-11.0))
                .flex()
                .items_center()
                .gap(px(2.0))
                .p(px(3.0))
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    this.set_change_row_hover(&hover_path, *hovered, true, cx);
                })),
            theme::shadow::POPOVER,
        )
        .child(
            icon_button(
                format!("change-row-open-in-editor-{}", path.display()),
                editor_selector,
                theme::changes::HOVER_ACTION_HOVER_BG,
                theme::text::SELECTED,
            )
            .tooltip(text_tooltip("Open in the editor"))
            .child("\u{2197}")
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                // Never also the row's own click handler: that would open the *diff* underneath
                // the File view this just asked for.
                cx.stop_propagation();
                let absolute = this.diff_root.join(&editor_path);
                this.open_file_view(absolute, window, cx);
            })),
        )
        .child(
            div()
                .flex_none()
                .w(px(1.0))
                .h(px(14.0))
                .bg(theme::border::POPOVER),
        );

        if discard_armed {
            bar.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "change-row-discard-confirm-{}",
                        path.display()
                    )))
                    .debug_selector(move || discard_selector)
                    .flex_none()
                    .h(px(22.0))
                    .px(px(9.0))
                    .flex()
                    .items_center()
                    .rounded(theme::radius::CARD_SM)
                    .cursor_pointer()
                    .bg(theme::changes::DISCARD_BG)
                    .border_1()
                    .border_color(theme::changes::DISCARD_BORDER)
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.0))
                    .text_color(theme::changes::DISCARD_FG)
                    .tooltip(text_tooltip(
                        "Click again to discard \u{2014} this cannot be undone",
                    ))
                    .child("Discard?")
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.discard_change_row(discard_path.clone(), cx);
                    })),
            )
            .into_any_element()
        } else {
            bar.child(
                icon_button(
                    format!("change-row-discard-{}", path.display()),
                    discard_selector,
                    theme::changes::DISCARD_HOVER_BG,
                    theme::changes::DISCARD_FG,
                )
                .tooltip(text_tooltip("Discard this file's changes"))
                .child("\u{21ba}")
                .on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.change_row_discard_armed = Some(arm_path.clone());
                        cx.notify();
                    },
                )),
            )
            .into_any_element()
        }
    }

    /// The one writer of both halves of §4i's hover state, and of the `Discard?` arming it
    /// governs. `from_bar` says which hitbox reported - see [`Self::change_row_hover`]'s docs for
    /// why there are two and why they must be independent.
    pub(in crate::sidebar) fn set_change_row_hover(
        &mut self,
        path: &Path,
        hovered: bool,
        from_bar: bool,
        cx: &mut Context<Self>,
    ) {
        let slot = if from_bar {
            &mut self.change_row_actions_hover
        } else {
            &mut self.change_row_hover
        };
        let changed = if hovered {
            let already = slot.as_deref() == Some(path);
            if !already {
                *slot = Some(path.to_path_buf());
            }
            !already
        } else if slot.as_deref() == Some(path) {
            *slot = None;
            true
        } else {
            false
        };
        if !changed {
            return;
        }
        let still_hovered =
            self.change_row_hover.is_some() || self.change_row_actions_hover.is_some();
        if !still_hovered {
            self.change_row_discard_armed = None;
        } else if let Some(armed) = self.change_row_discard_armed.clone() {
            let armed_still_hovered = self.change_row_hover.as_deref() == Some(armed.as_path())
                || self.change_row_actions_hover.as_deref() == Some(armed.as_path());
            if !armed_still_hovered {
                self.change_row_discard_armed = None;
            }
        }
        cx.notify();
    }

    /// The second click of §4i's `Discard?` confirm: a real, immediate
    /// `wt_core::stage::discard_path` for one file, on the background executor like every other
    /// real git mutation in this app.
    pub(in crate::sidebar) fn discard_change_row(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.change_row_discard_armed = None;
        self.changes_row_error = None;
        cx.notify();

        let worktree_path = self.diff_root.clone();
        let git_path = path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::stage::discard_path(&worktree_path, &git_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Err(err) => {
                        this.changes_row_error = Some((path, format!("failed to discard: {err}")));
                        cx.notify();
                    }
                    Ok(()) => {
                        // The file is no longer what the panel is drawing, and possibly no longer
                        // in it at all - re-read rather than guess.
                        this.load_diff(this.diff_root.clone(), cx);
                    }
                }
            });
        });
        self._discard_tasks.push(task);
    }

    /// `V`'s action handler ([`crate::root::ToggleChangeSeen`]) - `STAGE-A-CHANGELOG.md` §4i:
    /// "opening a file marks it seen […] and `V` unmarks".
    pub(crate) fn handle_toggle_change_seen_action(
        &mut self,
        _action: &crate::root::ToggleChangeSeen,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.open_uncommitted_change().cloned() else {
            return;
        };
        let Some(entry) = self.uncommitted_change_set.entry(&path) else {
            return;
        };
        let stat = entry.stat();
        // `seen_files` only - never `staged_files`. Audit I3: the checkbox owns staged, the name
        // owns seen, and neither reads the other.
        if self.seen_files.is_seen(&self.diff_root, &path, stat) {
            self.seen_files.clear(&self.diff_root, &path);
        } else {
            self.seen_files.mark_seen(&self.diff_root, &path, stat);
        }
        cx.notify();
    }

    /// The open file ([`Self::open_change`]) *as an Uncommitted-section row*, or `None` when
    /// nothing is open or what is open has no live uncommitted delta.
    pub(in crate::sidebar) fn open_uncommitted_change(&self) -> Option<&PathBuf> {
        let path = self.open_change.as_ref()?;
        self.uncommitted_change_set
            .entry(path)
            .is_some()
            .then_some(path)
    }

    /// Whether `V` really does something right now - what
    /// [`render_changes_footer`]'s keycap hint is gated on, so the strip never advertises a
    /// shortcut that would no-op. Same honesty rule [`render_file_tree_footer`]'s own `live`
    /// parameter implements for `F2`.
    pub(in crate::sidebar) fn change_seen_toggle_live(&self) -> bool {
        self.open_uncommitted_change().is_some()
    }

    /// `space`'s action handler ([`crate::root::ToggleChangeStaged`]) - the binding behind
    /// `Jerry.dc.html`'s `changesHints` first hint, `space stage` (line 4548).
    pub(crate) fn handle_toggle_change_staged_action(
        &mut self,
        _action: &crate::root::ToggleChangeStaged,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.open_uncommitted_change().cloned() else {
            return;
        };
        // `staged_files` only - never `seen_files`. Audit I3, the same rule
        // `handle_toggle_change_seen_action` states from the other side: the checkbox owns staged,
        // the name owns seen, and neither reads the other.
        self.toggle_staged(path, cx);
    }

    /// Whether `space` really does something right now - the gate on [`render_changes_footer`]'s
    /// `space stage` hint, and the same honesty rule [`Self::change_seen_toggle_live`] implements
    /// for `V`. Reads the same [`Self::open_uncommitted_change`] the action itself does, so the
    /// hint is on screen exactly when the keystroke would really stage or unstage something.
    pub(in crate::sidebar) fn change_stage_toggle_live(&self) -> bool {
        self.open_uncommitted_change().is_some()
    }

    /// Whether `alt`+click really has something to act on right now - the gate on
    /// [`render_changes_footer`]'s `⌥click filter by author` hint, and the exact same honesty rule
    /// [`Self::change_seen_toggle_live`] implements for `V`.
    pub(in crate::sidebar) fn change_author_filter_live(&self) -> bool {
        self.worktree_has_multiple_agents()
            && self
                .uncommitted_change_set
                .entries()
                .iter()
                .any(crate::provenance::render::has_drawable_author)
    }

    /// §4i's own wording for a filename that **is** seen, verbatim, including the `V` it names as
    /// the way back. Kept as a constant so the row's tooltip and the test that pins it against
    /// the design read the same string.
    pub(in crate::sidebar) const SEEN_TOOLTIP: &'static str =
        "Seen since the agent last changed it \u{2014} V to unmark";
    /// The unseen half of [`Self::SEEN_TOOLTIP`], also verbatim.
    pub(in crate::sidebar) const UNSEEN_TOOLTIP: &'static str =
        "Not seen since the agent last changed it \u{2014} opening it marks it seen";

    /// The loaded `WorktreeDiff` behind one file section's rows - the Uncommitted section's
    /// working-tree-against-`HEAD` diff, or the Against-main section's merge-base diff. `None` for
    /// the two sections that draw no file rows at all.
    fn diff_for(&self, section: sections::ChangesSection) -> Option<&WorktreeDiff> {
        match section {
            sections::ChangesSection::Uncommitted => self.uncommitted_diff.loaded(),
            sections::ChangesSection::AgainstMain => self.current_diff(),
            sections::ChangesSection::Runs | sections::ChangesSection::Commits => None,
        }
    }

    /// The Changes row's 12×12 staging checkbox (Revision R12 §5: the checkbox **is** staging,
    /// not "reviewed") - toggled via [`Self::toggle_staged`]. Stops propagation on click so
    /// checking a box never also opens the row's diff, mirroring `Self::render_agent_tab`'s
    /// nested-clickable-child pattern (its tab-close `×`).
    pub(in crate::sidebar) fn render_staging_checkbox(
        &self,
        path: PathBuf,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selector = format!("stage-checkbox-{}", path.display());
        div()
            .id(format!("stage-checkbox-{}", path.display()))
            .debug_selector(move || selector)
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
            .hover(|el| el.border_color(theme::toggle::CHECKBOX_HOVER))
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(9.0))
            .text_color(theme::button::GREEN_FG)
            .when(checked, |el| el.child("\u{2713}"))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.toggle_staged(path.clone(), cx);
            }))
    }

    /// The commit composer at the foot of the Changes panel (Revision R12 §5): header row
    /// (`COMMIT` · `N of M staged` · staged diffstat), a pre-drafted message box, and the primary
    /// commit action with its `▾` split-button menu. The staged set is derived once, early
    /// (`changes::staged_subset`), so the header count/diffstat/message/action-button all read
    /// from the exact same list, per the design's own "derive the staged set once" rule.
    pub(in crate::sidebar) fn render_commit_composer(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The **uncommitted** scope, not the merge-base diff this used to read (GitHub issue
        // #285). A commit composer's denominator is what is dirty in the checkout; the merge-base
        // diff also lists files whose only difference from `main` is already committed, which is
        // why it needed `changes::stageable_count` to subtract them back out again. That
        // subtraction is gone with the need for it - everything in this list is genuinely
        // stageable, by construction of the scope.
        let uncommitted: &[DiffFile] = self
            .uncommitted_diff
            .loaded()
            .map(|diff| diff.files.as_slice())
            .unwrap_or(&[]);
        let staged = changes::staged_subset(uncommitted, &self.staged_files);
        let staged_count = staged.len();
        let total = uncommitted.len();
        let (add, del) = changes::staged_diff_stats(&staged);
        let stat_text = if staged_count > 0 {
            format!("+{add} \u{2212}{del}")
        } else {
            String::new()
        };

        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == self.diff_root)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| "(detached)".to_string());

        // The single source of truth every commit path writes - the user's own typed text, and
        // nothing else (see `Self::staged_commit_message`'s own docs for the auto-drafted
        // fallback this used to have and why it was removed).
        let message = self.staged_commit_message();

        let busy = self.worktree_history_op_in_flight.is_some();
        let committing = self.worktree_history_op_in_flight
            == Some(worktree_history::WorktreeHistoryOpKind::Commit);
        let can_commit = staged_count > 0 && !message.trim().is_empty() && !busy;
        let label = if committing {
            "committing\u{2026}".to_string()
        } else {
            changes::commit_button_label(staged_count)
        };
        // Visual state (green vs. ghost) tracks the same "would this click actually do
        // something" fact `can_commit` gates on, *except* busy - the same "keep the enabled look
        // while a busy label shows" precedent `Self::render_footer_action_button` already follows
        // for its own `discarding…`/`keeping…` busy labels. Staged-but-no-message deliberately
        // stays ghost, not green: a green, clickable-looking button that silently no-ops without
        // a message would be exactly the anti-pattern this composer's own docs warn against.
        let ready_to_commit = staged_count > 0 && !message.trim().is_empty();
        let (primary_bg, primary_border, primary_fg): (gpui::Rgba, gpui::Rgba, gpui::Rgba) =
            if ready_to_commit {
                (
                    theme::button::GREEN_BG.into(),
                    theme::button::GREEN_BG.into(),
                    theme::button::GREEN_FG.into(),
                )
            } else {
                (
                    work_surface::TRANSPARENT,
                    // No exact token for the mockup's disabled-state `#262b30` - reused
                    // `theme::border::BUTTON` (`#2a2f34`, the closest ported outline-button
                    // border), the same reuse-nearest-token convention
                    // `work_surface::state::tab_colors`'s own docs establish for
                    // `theme::text::DIMMER` standing in for the design's `#767d84`.
                    theme::border::BUTTON.into(),
                    theme::text::FAINTER.into(),
                )
            };
        // No `⌘⏎`/`Ctrl+Enter` keycap on the primary button: the mockup shows one, but this app
        // has no real global keybinding for it - the same "app-level shortcut steals terminal
        // input" risk `work_surface::state::footer_actions`'s own `Keep all` row docs this exact
        // omission for (this app's whole domain is running agent CLIs in terminals, and
        // Ctrl+Enter/Cmd+Enter is a plausible "submit" gesture one of them could reasonably use
        // itself). That row's own docs are explicit that an audit once caught a keycap still
        // rendering after a promotion to "real" with no binding behind it - "a real keycap must
        // never render for a keystroke that does nothing" - so this composer never grows one in
        // the first place.

        // Test-only (no-op outside `cfg(test)`/`test-support`, matching every other
        // `debug_selector` in this file - see e.g. `Self::render_status_zoom_value`'s own
        // comment): the real staged/total counts baked directly into the selector string, so a
        // real interaction test can prove this header reflects real `staged_subset`/diff state
        // rather than a hardcoded string, without GPUI needing a general "read back the painted
        // text" API.
        let header_selector = format!("commit-composer-progress-{staged_count}-of-{total}");
        let stat_selector = format!("commit-composer-stat-{stat_text}");
        let branch_selector = format!("commit-composer-branch-{branch}");
        let message_selector = if message.is_empty() {
            "commit-composer-message-empty".to_string()
        } else {
            format!("commit-composer-message-{message}")
        };

        let composer = div()
            .id("commit-composer")
            .debug_selector(|| "commit-composer".to_string())
            .relative()
            .flex_none()
            .flex()
            .flex_col()
            .px(px(12.0))
            .pt(px(9.0))
            .pb(px(10.0))
            .border_t_1()
            .border_color(theme::border::INNER)
            .bg(theme::surface::FOOTER)
            .child({
                // GitHub issue #176: the `▾` popover is no longer a child of this composer (see
                // `AdeApp::commit_composer_bounds`' own docs), so it needs this composer's real
                // window-space bounds to anchor off - the same `gpui::canvas` capture
                // `crate::work_surface::render::AdeApp::render_tab_strip_plus` uses for the `+`
                // button.
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.commit_composer_bounds = bounds;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                // Explicit insets rather than `.size_full()`: an absolutely-positioned child with
                // no insets takes its *static* position, which here is the composer's content box
                // (inside the 12px horizontal padding) while `size_full` still resolves against
                // the padding box - so the captured origin and size would describe two different
                // rectangles. Pinning all four edges makes this exactly the composer's own box,
                // which is what `render_commit_menu` then insets by the design's 12px.
                .top(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .bottom(px(0.0))
            })
            .child(
                // Header row: `COMMIT` · `N of M staged` · staged diffstat, right-aligned.
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pb(px(7.0))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(9.5))
                            .text_color(theme::palette::GROUP_HEADER)
                            .child("COMMIT"),
                    )
                    .child(
                        div()
                            .debug_selector(move || header_selector)
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::DIM)
                            .child(format!("{staged_count} of {total} staged")),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .debug_selector(move || stat_selector)
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::diff::STAT_ADD)
                            .child(stat_text),
                    ),
            )
            .child(
                // The message box - a normal text input the user fills in themselves, nothing
                // pre-drafted (see `Self::staged_commit_message`'s own docs).
                self.wire_text_input_actions(
                    div()
                        .id("commit-composer-message")
                        .debug_selector(|| "commit-composer-message-field".to_string())
                        .flex_none()
                        .border_1()
                        .border_color(theme::border::CARD)
                        .rounded(theme::radius::CARD_SM)
                        .bg(theme::surface::CARD_SUNK)
                        .px(px(9.0))
                        .py(px(7.0))
                        .track_focus(&self.commit_message_focus_handle)
                        .key_context("text-input")
                        .on_action(cx.listener(Self::handle_commit_message_text_undo))
                        .on_action(cx.listener(Self::handle_commit_message_text_redo))
                        .on_key_down(cx.listener(Self::handle_commit_message_key_down)),
                    commit_message_handle(),
                    cx,
                )
                .cursor_text()
                // Live report ("carret is not centered verticaly, when typing it goes to the
                // right side of the input"): this row used to be `.items_start()` with
                // `.flex_1().min_w_0()` on the *text* div below. `flex_1` stretched the text
                // div's layout box across the whole field, so the caret - a `flex_none`
                // sibling rendered *after* it in DOM order once the message is non-empty -
                // sat pinned at the field's right edge instead of adjacent to the last glyph,
                // and `items_start` top-aligned the 14px caret bar against the 17px text
                // line. Now the exact structure every working simple input uses
                // (`Self::render_rail_filter_row`'s caret+text wrapper, `Self::
                // render_new_file_prompt`'s name box): an `items_center` row whose text div is
                // intrinsically sized, so the caret sits right next to the real text.
                //
                // No decorative gap before the caret - see
                // `crate::rail::render::AdeApp::render_rail_filter_row`'s own comment for why
                // (live report: it read as a gap between the typed text and where it's
                // actually being typed). This field was written while that 2px gap was still
                // on every other input in the app; it goes here for the same reason it went
                // everywhere else.
                .flex()
                .items_center()
                .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                    this.focus_commit_message(window, cx);
                }))
                // Mirrors `Self::render_rail_filter_row`'s own fix exactly (GitHub issue #45):
                // a caret pinned unconditionally *after* this child sits glued to the end of
                // the placeholder when the field is empty, instead of at the real cursor
                // position (0, before any text at all). It belongs before the placeholder
                // when empty and after the real message once there is any.
                // GitHub issue #336: through the one helper that owns this structure, like
                // every other simple input in the app - which is also what gives the composer
                // a real caret *position* (it used to pin the bar to the end of the message
                // whatever the user had arrowed back to), a selection highlight, and real
                // click-to-position.
                .child(self.render_simple_input_row(
                    SimpleInput {
                        caret_selector: "commit-composer-message-caret".into(),
                        text_selector: SharedString::from(message_selector),
                        focus_handle: Some(&self.commit_message_focus_handle),
                        text: &message,
                        caret_offset: self.commit_message.caret(),
                        selection: self.commit_message.selection(),
                        placeholder: "commit message",
                        font: theme::font::SANS,
                        text_size: px(11.5),
                        text_color: theme::text::STRONG,
                        placeholder_color: theme::text::FAINT,
                        caret: SimpleInputCaret::default(),
                        field: Some(commit_message_handle()),
                    },
                    cx,
                )),
            )
            .child(
                // Primary action + split-button menu + right-aligned target branch.
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pt(px(8.0))
                    .child({
                        let mut button = div()
                            .id("commit-composer-primary")
                            .debug_selector(|| "commit-composer-primary".to_string())
                            .flex_none()
                            .h(px(24.0))
                            .px(px(10.0))
                            .rounded(theme::radius::BUTTON)
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .bg(primary_bg)
                            .border_1()
                            .border_color(primary_border)
                            .child(
                                div()
                                    .font(font(theme::font::SANS))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_size(px(11.0))
                                    .text_color(primary_fg)
                                    .child(label),
                            );
                        button = if can_commit {
                            button
                                .cursor_pointer()
                                .hover(|el| el.bg(theme::button::GREEN_BG_HOVER))
                                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                    this.commit_staged_files(cx);
                                }))
                        } else {
                            button.cursor_default()
                        };
                        button
                    })
                    .child(
                        div()
                            .id("commit-composer-menu-toggle")
                            .debug_selector(|| "commit-composer-menu-toggle".to_string())
                            .flex_none()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded(theme::radius::BUTTON)
                            .border_1()
                            .border_color(theme::border::BUTTON)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                            .font(font(theme::font::MONO))
                            .text_size(px(8.5))
                            .text_color(theme::text::DIMMER)
                            .child("\u{25be}")
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                let opening = !this.commit_menu_open;
                                // GitHub issue #176 - see `AdeApp::close_menu_surfaces_except`.
                                let _ = this
                                    .close_menu_surfaces_except(Some(menus::MenuSurface::Commit));
                                this.commit_menu_open = opening;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().min_w_0())
                    .child(
                        div()
                            .debug_selector(move || branch_selector)
                            .flex_none()
                            .max_w(px(120.0))
                            .overflow_hidden()
                            .truncate()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::PATH)
                            .child(branch),
                    ),
            );

        composer
    }

    /// The commit composer's `▾` split-button popover (Revision R12 §5): *Commit and push* /
    /// *Commit all files* / *Amend last commit* / *Stash staged files*.
    pub(crate) fn render_commit_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // The composer's own painted box, in window space. The popover keeps its R12 §5 side
        // geometry relative to that box - inset 12px on both sides - and hangs 1px below its
        // bottom edge, so it reads as belonging to the `▾` it came from rather than floating.
        let composer = self.commit_composer_bounds;
        let popover_left = composer.origin.x + px(12.0);
        let popover_width = (composer.size.width - px(24.0)).max(px(0.0));
        let popover_top = composer.origin.y + composer.size.height + px(1.0);

        let branch = self
            .composer_branch()
            .unwrap_or_else(|| "(detached)".to_string());
        let total = self
            .uncommitted_diff
            .loaded()
            .map(|diff| diff.files.len())
            .unwrap_or(0);

        div()
            .id("commit-menu-scrim")
            .debug_selector(|| "commit-menu-scrim".to_string())
            .absolute()
            // Starts *below* the title bar, exactly like the file tree's own context-menu scrim
            // (`Self::render_tree_context_menu`) and for the same real reason it documents: a
            // full-window `.occlude()`ing layer swallows the window's own close/minimise/maximise
            // caption buttons and the title bar's drag region, so the window could not be closed
            // or moved while the menu was up.
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            // Without this, `Window::hit_test` (`vendor/zed/crates/gpui/src/window.rs`) keeps
            // walking *underneath* this transparent overlay and includes whatever real button
            // sits at the same screen position as still "hovered" - so a click that lands on,
            // say, the still-visible `Self::render_commit_composer` menu-toggle button beneath
            // this scrim would fire *both* the scrim's own close handler *and* that button's
            // real handler in the same click. `.occlude()` is the same fix
            // `root::resize::render_resize_handle`'s own handle already uses for exactly this
            // "receives the mouse, not whatever's underneath" reason. Proved genuinely necessary
            // (not cargo-culted) by `commit_composer_tests::
            // opening_the_commit_menu_blocks_a_real_click_on_the_primary_button_underneath` and
            // `commit_composer_tests::clicking_the_menu_toggle_genuinely_opens_and_closes_the_
            // commit_menu`, both of which fail without it.
            .occlude()
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.commit_menu_open = false;
                cx.notify();
            }))
            // A right-click anywhere else must dismiss too - the same rule the file tree's and the
            // graph row's own scrims already follow, so the next right-click doesn't land on an
            // invisible layer and appear to do nothing at all.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _event: &gpui::MouseDownEvent, _window, cx| {
                    this.commit_menu_open = false;
                    cx.notify();
                }),
            )
            .child(
                menu_popover_chrome(
                    div()
                        .id("commit-menu-popover")
                        .debug_selector(|| "commit-menu-popover".to_string())
                        .absolute()
                        .left(popover_left)
                        .w(popover_width)
                        // The scrim starts at `theme::band::TITLE_BAR`, not at the window top, so
                        // a `top` measured in window space has to have that offset taken back off
                        // - the scrim's own origin *is* `TITLE_BAR`, and an absolutely-positioned
                        // child resolves against its positioned ancestor, not the window.
                        .top(popover_top - theme::band::TITLE_BAR)
                        // Kept from when this popover lived inside the composer: it paints over
                        // real Changes rows and the primary commit button, and blocking the mouse
                        // structurally (rather than relying on bubble-phase listener ordering)
                        // is what stops a click on the panel reaching them.
                        .occlude()
                        .py(px(4.0)),
                    // `MENU`, like every other downward-opening popover in the app, now that
                    // this one opens downward too - `COMMIT_MENU`'s negative `y` existed only for
                    // the upward direction this popover no longer has.
                    theme::shadow::MENU,
                )
                .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                }))
                .children(CommitMenuAction::ORDER.map(|action| {
                    self.render_commit_menu_row(action, action.hint(&branch, total), cx)
                })),
            )
    }
}

/// The four real actions the commit composer's `▾` split-button menu offers, each backed by a real
/// `wt_core` call - see [`AdeApp::run_commit_menu_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitMenuAction {
    /// `wt_core::undo::commit_paths` then `wt_core::remote::push_branch`.
    CommitAndPush,
    /// `wt_core::undo::commit_all_changes` - `git add -A` and commit, not just the staged subset.
    CommitAllFiles,
    /// `wt_core::undo::amend_head_with_paths` - folds the staged paths into the tip, keeping its
    /// message.
    AmendLastCommit,
    /// `wt_core::undo::stash_staged` - `git stash push --staged`, leaving unstaged work alone.
    StashStaged,
}

impl CommitMenuAction {
    /// Top to bottom, in the order the mock lists them.
    pub(crate) const ORDER: [CommitMenuAction; 4] = [
        CommitMenuAction::CommitAndPush,
        CommitMenuAction::CommitAllFiles,
        CommitMenuAction::AmendLastCommit,
        CommitMenuAction::StashStaged,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            CommitMenuAction::CommitAndPush => "Commit and push",
            CommitMenuAction::CommitAllFiles => "Commit all files",
            CommitMenuAction::AmendLastCommit => "Amend last commit",
            CommitMenuAction::StashStaged => "Stash staged files",
        }
    }

    /// The row's sub-label when the action really can run - what it will *do*, in the real terms
    /// of this worktree (its branch, its file count), never a generic sentence.
    pub(crate) fn hint(self, branch: &str, uncommitted_files: usize) -> String {
        match self {
            CommitMenuAction::CommitAndPush => format!("origin/{branch}"),
            CommitMenuAction::CommitAllFiles => format!(
                "stages the rest first \u{2022} {}",
                plural::count(uncommitted_files, "file", None)
            ),
            CommitMenuAction::AmendLastCommit => "rewrites the tip".to_string(),
            CommitMenuAction::StashStaged => "keeps the worktree clean".to_string(),
        }
    }

    /// The stable id/selector fragment for this row.
    pub(crate) fn key(self) -> &'static str {
        match self {
            CommitMenuAction::CommitAndPush => "commit-and-push",
            CommitMenuAction::CommitAllFiles => "commit-all-files",
            CommitMenuAction::AmendLastCommit => "amend-last-commit",
            CommitMenuAction::StashStaged => "stash-staged-files",
        }
    }
}

impl AdeApp {
    /// One row of [`AdeApp::render_commit_menu`]'s split-button popover - label + sub-label, no
    /// leading chip (unlike `crate::work_surface::render::render_dropdown_menu_row`, which this
    /// deliberately doesn't reuse: the design has no per-row glyph here).
    fn render_commit_menu_row(
        &self,
        action: CommitMenuAction,
        hint: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let availability = self.commit_menu_availability(action);
        let enabled = availability.is_ok();
        let sub = match &availability {
            Ok(()) => hint,
            Err(reason) => reason.clone(),
        };
        let selector = format!("commit-menu-row-{}", action.key());
        let state_selector = format!(
            "commit-menu-row-{}-{}",
            action.key(),
            if enabled { "enabled" } else { "disabled" }
        );

        div()
            .id(gpui::SharedString::from(selector))
            .debug_selector(move || state_selector)
            .flex()
            .flex_col()
            .gap(px(1.0))
            .px(px(10.0))
            .py(px(5.0))
            .when(enabled, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.run_commit_menu_action(action, cx);
                    }))
            })
            .when(!enabled, |el| el.cursor_default().opacity(0.5))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        theme::text::HEADING
                    } else {
                        theme::text::GHOSTER
                    })
                    .child(action.label()),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(9.5))
                    .text_color(theme::text::FAINTER)
                    .child(sub),
            )
    }
}

/// Which data source the right sidebar currently shows for the selected worktree - Zone 3's
/// `right_pane` state (`Files · Search · Changes`, `Files` default). The panel never shows diff
/// *content* (see [`AdeApp::open_change`]'s docs) - `Changes` is the per-file review list,
/// not a diff view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RightSidebarView {
    Files,
    Search,
    Changes,
}

/// The next tab in `Files -> Search -> Changes -> Files` order, behind the command palette's
/// `Cycle Right Panel`.
pub(crate) fn next_right_sidebar_view(current: RightSidebarView) -> RightSidebarView {
    match current {
        RightSidebarView::Files => RightSidebarView::Search,
        RightSidebarView::Search => RightSidebarView::Changes,
        RightSidebarView::Changes => RightSidebarView::Files,
    }
}

/// One row of [`AdeApp::render_file_tree`]'s virtualized list: either a real walked entry (by
/// index into [`AdeApp::file_tree`]) or the in-progress inline name editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreeRow {
    Entry(usize),
    InlineEditor,
}

impl AdeApp {
    /// The file tree's right-click context menu popover (GitHub issue #19 §1) - the app's one
    /// shared menu ([`AdeApp::render_menu_overlay`]), given the file tree's own rows.
    pub(crate) fn render_tree_context_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let menu = self.tree_context_menu.clone();
        let rows = menu
            .as_ref()
            .map(|menu| context_menu::menu_rows(&menu.target, self.tree_clipboard.is_some()))
            .unwrap_or_default();
        let origin_x = menu.as_ref().map(|menu| menu.origin_x).unwrap_or(0.0);
        let origin_y = menu.as_ref().map(|menu| menu.origin_y).unwrap_or(0.0);

        self.render_menu_overlay(
            crate::menu::render::MenuOverlay {
                id: "tree-context-menu",
                origin_x,
                origin_y,
                rows,
                on_pick: |this, action, window, cx| this.run_tree_menu_action(action, window, cx),
                on_dismiss: |this, cx| this.close_tree_context_menu(cx),
            },
            cx,
        )
    }
}

/// The Changes list's footer 29 - **three keycap hints and no prose at all**, exactly as
/// `design_handoff_jerry_ade/revision 5/Jerry.dc.html` line 4548 declares it:
/// `changesHints: this.mkHints([['space', 'stage'], ['V', 'mark seen'], ['⌥click', 'filter by
/// author']])`, rendered through the same `diffHints` hint-row template (line 842) that is purely a
/// loop over keycap+label pairs. `STAGE-A-CHANGELOG.md` §2's ride-along I10 lists the same strip.
pub(in crate::sidebar) fn render_changes_footer(
    text_size: Pixels,
    stage_toggle_live: bool,
    seen_toggle_live: bool,
    macos: bool,
    author_filter_live: bool,
) -> impl IntoElement {
    // One hint, the mock's own `[keycaps] label` pair - the shape `render_file_tree_footer`'s
    // identical local `hint` closure builds beside it, rather than three near-copies of the same
    // eleven lines.
    let hint = move |selector: &'static str, parts: Vec<String>, label: &'static str| {
        div()
            .id(selector)
            .debug_selector(move || selector.to_string())
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .child(render_keycap_row(&parts, KeycapSize::Hint))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(text_size)
                    .text_color(theme::text::PATH)
                    .child(label),
            )
    };

    div()
        .debug_selector(|| "changes-footer".to_string())
        .flex_none()
        .h(theme::band::SURFACE_FOOTER)
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(11.0))
        .border_t_1()
        .border_color(theme::border::INNER)
        .bg(theme::surface::FOOTER)
        .font(font(theme::font::MONO))
        .text_size(text_size)
        .text_color(theme::text::HINT)
        // The mock's first hint. Gated on there being an Uncommitted row open to stage - see
        // `AdeApp::handle_toggle_change_staged_action` for why that scope and not merely "a file
        // is open".
        .when(stage_toggle_live, |el| {
            el.child(hint(
                "changes-footer-stage-hint",
                // Not the literal string `"space"`: resolved through the same
                // `keymap::resolve_combo` every other real keycap in this app goes through, off a
                // spec named once in [`CHANGES_STAGE_SPEC`] so the keycap and the registered
                // binding cannot drift.
                keymap::resolve_combo(CHANGES_STAGE_SPEC, macos),
                "stage",
            ))
        })
        // The mock's second hint, and `STAGE-A-CHANGELOG.md` §4i's own legend entry.
        .when(seen_toggle_live, |el| {
            el.child(hint(
                "changes-footer-seen-hint",
                keymap::resolve_combo(CHANGES_SEEN_SPEC, macos),
                "mark seen",
            ))
        })
        // The mock's third hint, `⌥click filter by author`, as real keycaps rather than the prose
        // `alt+click` (ride-along I10). Gated on the affordance genuinely being on screen:
        // `alt`+click filters by clicking an *author chip*, and chips only exist in a multi-agent
        // worktree (`REVISION-2026-07-31.md` §4), so in a one-agent worktree this would advertise
        // a gesture with nothing to perform it on.
        .when(author_filter_live, |el| {
            el.child(hint(
                "changes-footer-author-filter-hint",
                keymap::resolve_combo(crate::provenance::render::AUTHOR_FILTER_SPEC, macos),
                "filter by author",
            ))
        })
}

/// The `crate::keymap::resolve_combo` spec [`render_changes_footer`] advertises for `space`, named
/// so its own test can assert it really is a registered binding rather than a plausible string -
/// exactly like [`CHANGES_SEEN_SPEC`] and [`FILE_TREE_RENAME_SPEC`] beside it. Passes through
/// `resolve_token` unchanged on both platforms, which is what the mock prints: a `space` keycap.
pub(in crate::sidebar) const CHANGES_STAGE_SPEC: &str = "space";

/// The `crate::keymap::resolve_combo` spec [`render_changes_footer`] advertises for `V`, named so
/// its own test can assert it really is a registered binding rather than a plausible string -
/// exactly like [`FILE_TREE_RENAME_SPEC`] beside it.
pub(in crate::sidebar) const CHANGES_SEEN_SPEC: &str = "v";

/// The Files tree's keyboard-hint footer - the counterpart to [`render_changes_footer`] beside
/// it, so switching between the two sidebar views with a diff loaded doesn't move the list under
/// the cursor. (The Changes view's *no-diff* arm still has no footer; that arm renders a single
/// message rather than a list, so there is nothing for a footer to sit under.)
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
        .when(live, |el| el.child(hint(FILE_TREE_RENAME_SPEC, "rename")))
}

/// The `crate::keymap::resolve_combo` spec [`render_file_tree_footer`] advertises, named so its
/// own test can assert it really is a registered binding rather than a plausible string.
pub(in crate::sidebar) const FILE_TREE_RENAME_SPEC: &str = "F2";

/// GitHub issue #148: what a real file-tree row drag carries - every path the gesture moves
/// (the whole real selection, if the dragged row was already part of one; just that one row
/// otherwise - see `AdeApp::render_file_tree_row`'s own `.on_drag` for which). Mirrors
/// `crate::work_surface::render::DraggedTab`'s exact shape (a small `Render`ed chip showing what's
/// being dragged) for the tab strip's own drag-and-drop, not a second, differently-behaved
/// mechanism - GPUI's own `on_drag`/`on_drag_move`/`on_drop` triple is the same either way.
#[derive(Clone)]
pub(in crate::sidebar) struct TreeDragPayload {
    pub(in crate::sidebar) paths: Vec<PathBuf>,
    label: String,
}

impl gpui::Render for TreeDragPayload {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .opacity(0.85)
            .px(px(10.0))
            .py(px(4.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::PALETTE)
            .border_1()
            .border_color(theme::border::POPOVER)
            .font(font(theme::font::SANS))
            .text_size(px(11.0))
            .text_color(theme::text::BODY)
            .child(self.label.clone())
    }
}

/// The file tree row's `▾`/`▸` caret, signaling a directory row is clickable/expandable,
/// distinct from the folder icon itself. Blank but still 8px wide for a file row, to keep
/// every row's icon column aligned.
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

/// The Changes row's `moved` tag for a real rename (`changes::is_real_rename`) - its own muted
/// chip rather than a fourth [`changes::StatusLetter`], since `STAGE-A-CHANGELOG.md` §4j's table
/// is exactly three letters with three colours and a rename is not a fourth thing git did to the
/// file's *contents*. See `changes::status_letter`'s own docs for why `Renamed` maps to `M` and
/// this chip carries the rename instead.
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
        // A real feature branch, so this hits `wt_core::diff::DiffBase::Diff` against a real
        // merge-base rather than `DiffBase::NoBase`'s uncommitted-vs-HEAD fallback (GitHub issue
        // #108) - the same setup this crate's existing real-diff tests use, and the shape this
        // test's 40 changed rows need to exercise virtualization against.
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

    #[gpui::test]
    fn file_tree_row_and_header_actions_clear_the_real_scrollbar(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir(repo.path().join("src")).expect("mkdir");
        fs::write(repo.path().join("src/main.rs"), "//\n").expect("write");
        // Enough flat files that the tree genuinely overflows its viewport and the real overlay
        // scrollbar actually renders - `render_vertical_scrollbar` returns `None`, painting
        // nothing at all, when the list doesn't overflow (see that function's own early return
        // on `max_offset <= 0.5`).
        for index in 0..300 {
            fs::write(repo.path().join(format!("file-{index:03}.txt")), "x\n").expect("write");
        }
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let scrollbar = cx.debug_bounds("file-tree-scrollbar").expect(
            "the tree must genuinely overflow for this test to prove anything about the real \
             scrollbar's geometry - if this is `None` the precondition itself is broken",
        );

        // A real minimum gap - deliberately not derived from `CONTENT_CLEARANCE` itself (see
        // this test's own docs above).
        let min_gap = px(4.0);

        // Directories sort before files at the same depth (`file_tree`'s own sort), so "src" is
        // the tree's very first row - guaranteed to paint inside any plausible viewport,
        // regardless of virtualization.
        // `debug_bounds` takes `&'static str`; this codebase's established test-only pattern for
        // a selector that must embed a real runtime path (see `crate::root::focus`'s own tests)
        // is a deliberate, test-only leak via `Box::leak`.
        let row_selector: &'static str = Box::leak(
            format!("file-tree-new-file-{}", repo.path().join("src").display()).into_boxed_str(),
        );
        let row_button = cx
            .debug_bounds(row_selector)
            .expect("the \"src\" directory row's own \"+\" control must really paint");
        assert!(
            row_button.right() + min_gap <= scrollbar.left(),
            "the file tree row's own \"+\" control (right edge {:?}) must clear the real \
             scrollbar's own left edge ({:?}) by a real, visually distinct margin - not just \
             avoid literal pixel overlap, which is what collided in GitHub issue #123's \
             screenshot",
            row_button.right(),
            scrollbar.left(),
        );

        let header_button = cx.debug_bounds("file-tree-new-file-root").expect(
            "the header's own root \"New file\" control must really paint in the Files view",
        );
        assert!(
            header_button.right() + min_gap <= scrollbar.left(),
            "the header's action cluster (right edge {:?}) must also clear the real \
             scrollbar's own left edge ({:?}) by a real margin - this is the exact control \
             GitHub issue #123's screenshot shows colliding with the scrollbar",
            header_button.right(),
            scrollbar.left(),
        );
    }
}

/// The reported "double border left and bottom" applied to the Changes panel's own rows - see
/// `crate::graph_view::render::AdeApp::render_graph_row`'s own docs for the full GPUI
/// `Style::border_color`-is-one-shared-value explanation this fix is built on.
#[cfg(test)]
mod change_row_selection_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::process::Command;
    use tempfile::TempDir;

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

    #[gpui::test]
    fn the_selection_edge_is_a_real_element_painted_regardless_of_selection(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "base\n").expect("write");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "base\nchanged\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.current_diff().map(|d| d.files.len())),
            Some(1),
            "sanity check: the one real changed file must really be in the loaded diff"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            None,
            "premise: nothing is selected yet, so this genuinely exercises the unselected case"
        );

        let edge_unselected = cx.debug_bounds("change-row-a.txt-selection-edge").expect(
            "the selection-edge child must be painted even while the row is unselected - if \
             it's `None` here, the edge is still only created `.when(selected, ...)`, the exact \
             regression this test exists to catch",
        );

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(std::path::PathBuf::from("a.txt"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(std::path::PathBuf::from("a.txt")),
            "sanity check: the row really is selected now"
        );

        let edge_selected = cx
            .debug_bounds("change-row-a.txt-selection-edge")
            .expect("the selection-edge child must still be painted while the row is selected");

        assert_eq!(
            edge_unselected.origin, edge_selected.origin,
            "the selection edge's own position must never move - only its colour toggles \
             (unselected: {:?}, selected: {:?})",
            edge_unselected, edge_selected
        );
        assert_eq!(
            edge_unselected.size, edge_selected.size,
            "the selection edge's own size must never change (unselected: {:?}, selected: {:?})",
            edge_unselected, edge_selected
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
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
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
            app.file_tree
                .visible_entries(&app.expanded_dirs)
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

        let (app_b, cx) =
            open_app_with_state_dir(cx, repo_b.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();

        app_a.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo_a.path().join("src"), cx);
        });
        cx.run_until_parked();
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

        let (reloaded_a, cx) =
            open_app_with_state_dir(cx, repo_a.path().to_path_buf(), settings_path);
        cx.run_until_parked();
        assert_eq!(
            reloaded_a.read_with(cx, |app, _| expanded_names(app)),
            vec!["src".to_string()]
        );
    }

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
            app.file_tree.visible_entries(&app.expanded_dirs).len()
        });
        assert_eq!(
            visible, 801,
            "all 800 files plus the folder row itself are visible rows - the old 500-row cap \
             would have silently hidden 301 of them"
        );
        assert!(
            app.read_with(cx, |app, _| app.file_tree_complete),
            "the walk reached everything, so the listing is a complete inventory"
        );
        assert!(
            cx.debug_bounds("file-tree-show-all").is_none(),
            "the 'load more' action was removed outright (GitHub issue #160) and must never \
             render again"
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

    #[gpui::test]
    fn a_tree_past_the_removed_cap_loads_every_entry_with_no_load_more_row(
        cx: &mut TestAppContext,
    ) {
        const DIRS: usize = 200;
        const FILES_PER_DIR: usize = 105;
        const EXPECTED: usize = DIRS + DIRS * FILES_PER_DIR;

        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        for d in 0..DIRS {
            let sub = repo.path().join(format!("d-{d:03}"));
            fs::create_dir(&sub).expect("mkdir");
            for f in 0..FILES_PER_DIR {
                fs::write(sub.join(format!("f-{f:03}.txt")), "x\n").expect("write");
            }
        }

        let (app, cx) = open_app_with_state_dir(
            cx,
            repo.path().to_path_buf(),
            state_dir.path().join("settings.toml"),
        );
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree.len()),
            EXPECTED,
            "the walk used to stop at 20,000 entries - every folder and file must now load"
        );
        assert!(
            app.read_with(cx, |app, _| app.file_tree_complete),
            "and a walk that reached everything is a complete inventory, so fold-state pruning \
             may trust it"
        );
        assert!(
            cx.debug_bounds("file-tree-show-all").is_none(),
            "the 'Stopped at N entries - load more' row must not exist any more"
        );

        // The palette's own candidate list is derived from the same walk, on the background
        // executor now that it is unbounded - it must cover the whole tree, not the first 20,000.
        assert_eq!(
            app.read_with(cx, |app, _| app.palette_file_candidates.len()),
            DIRS * FILES_PER_DIR,
            "one candidate per file (directories are not candidates), across the whole tree"
        );

        // A folder well past the old cut-off must expand and render for real, not just be
        // counted: `d-199` starts at entry ~21,000.
        let last_dir = repo.path().join(format!("d-{:03}", DIRS - 1));
        app.update(cx, |app, cx| app.toggle_dir_expanded(last_dir.clone(), cx));
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| {
                app.file_tree
                    .visible_entries(&app.expanded_dirs)
                    .iter()
                    .any(|entry| entry.path == last_dir.join("f-104.txt"))
            }),
            "the last file of the last directory is ~1,200 entries past the removed cap and must \
             be a real, visible row"
        );
    }

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
        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_root.clone()),
            real.path(),
            "opening through a symlink must root the tree at the resolved path, so nothing this \
             app walks or spawns from it carries the unresolved one"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_palette_file_result(real.path().join("src/app/main.rs"), window, cx);
        });
        cx.run_until_parked();

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

    #[cfg(unix)]
    #[gpui::test]
    fn an_incomplete_walk_never_prunes_fold_state(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;

        let repo = TempDir::new().expect("tempdir");
        let state_dir = TempDir::new().expect("tempdir");
        let outer = repo.path().join("outer");
        fs::create_dir(&outer).expect("mkdir");
        fs::create_dir(outer.join("zzz")).expect("mkdir");
        fs::write(outer.join("zzz/inside.txt"), "x\n").expect("write");
        let settings_path = state_dir.path().join("settings.toml");
        let fold_path = state_dir.path().join("file-tree-state.toml");

        let (app, cx) =
            open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path.clone());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(outer.join("zzz"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            FoldState::load_at(&fold_path)
                .expanded_dirs(repo.path())
                .len(),
            1
        );

        fs::set_permissions(&outer, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read_dir(&outer).is_ok() {
            // Running as root (or on a filesystem that ignores the mode) - the premise doesn't
            // hold, so this would pass for the wrong reason.
            fs::set_permissions(&outer, fs::Permissions::from_mode(0o755)).expect("chmod back");
            return;
        }

        let (reloaded, cx) = open_app_with_state_dir(cx, repo.path().to_path_buf(), settings_path);
        cx.run_until_parked();

        let complete = reloaded.read_with(cx, |app, _| app.file_tree_complete);
        let saw_zzz = reloaded.read_with(cx, |app, _| {
            app.file_tree.iter().any(|entry| entry.name == "zzz")
        });
        fs::set_permissions(&outer, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(
            !complete,
            "precondition: the walk really must have come back incomplete, from a real \
             unreadable directory - not from a flag this test set itself"
        );
        assert!(
            !saw_zzz,
            "precondition: the expanded directory really must be absent from that listing"
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

    #[gpui::test]
    fn indent_guides_stay_neutral_even_when_a_descendant_file_is_open_and_focus_leaves_the_tree(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        let (app, cx) = open_deep_tree(cx, &repo);

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("a/b/c/deep.txt"), window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(
                !app.tree_focus_handle.is_focused(window),
                "premise: opening a file via the normal path focuses the editor, not the tree"
            );
        });
        for level in 0..3 {
            assert!(
                cx.debug_bounds(match level {
                    0 => "file-tree-guide-deep.txt-0",
                    1 => "file-tree-guide-deep.txt-1",
                    _ => "file-tree-guide-deep.txt-2",
                })
                .is_some(),
                "the open file's own ancestor guides must still render at level {level} - just \
                 never in a distinct colour"
            );
        }
        assert!(
            cx.debug_bounds("file-tree-guide-active-deep.txt-0")
                .is_none(),
            "no guide may ever render under the old accent-colour selector, even for the open \
             file's own chain, with focus on the editor"
        );

        app.update_in(cx, |app, window, cx| {
            app.focus_file_tree(window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-guide-active-deep.txt-0")
                .is_none(),
            "and still none once the tree regains focus - selecting/focusing the open file's own \
             row must not recolour its ancestor guides either"
        );
        assert!(
            cx.debug_bounds("file-tree-guide-deep.txt-0").is_some(),
            "the guide itself must still be there, just neutral"
        );
    }

    #[gpui::test]
    fn each_row_draws_one_guide_per_level_aligned_with_that_levels_chevron(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        let (_app, cx) = open_deep_tree(cx, &repo);

        assert!(
            cx.debug_bounds("file-tree-guide-a-0").is_none(),
            "a root-level row has no ancestors, so it must draw no guide at all"
        );

        let row = cx
            .debug_bounds("file-tree-row-deep.txt")
            .expect("the deepest row must paint");
        for (level, selector) in [
            (0usize, "file-tree-guide-deep.txt-0"),
            (1, "file-tree-guide-deep.txt-1"),
            (2, "file-tree-guide-deep.txt-2"),
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
            cx.debug_bounds("file-tree-guide-deep.txt-3").is_none(),
            "a depth-3 row must not draw a fourth guide"
        );
    }

    #[gpui::test]
    fn guides_on_consecutive_rows_join_with_no_gap(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let (_app, cx) = open_deep_tree(cx, &repo);

        // The three consecutive rows `c`, `deep.txt`, `mid.txt` (in that render order - `c`'s
        // own child comes between it and its sibling), all of which draw a level-1 guide.
        let segments: Vec<gpui::Bounds<Pixels>> = [
            "file-tree-guide-c-1",
            "file-tree-guide-deep.txt-1",
            "file-tree-guide-mid.txt-1",
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
            cx.debug_bounds("file-tree-guide-f-000.txt-0").is_none(),
            "the row that scrolled out of view must take its guides with it - a leftover guide \
             here would be a segment painted over an unrelated row"
        );

        for (level, selector) in [
            (0usize, "file-tree-guide-f-299.txt-0"),
            (1, "file-tree-guide-f-299.txt-1"),
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

    #[gpui::test]
    fn no_rows_guides_are_ever_recoloured_by_a_sibling_subtrees_open_file(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        // A second branch at the same depth as `a/b`, so there is a row that shares only a
        // partial ancestor chain with the file that ends up open.
        fs::create_dir_all(repo.path().join("a/other")).expect("mkdir");
        fs::write(repo.path().join("a/other/elsewhere.txt"), "x\n").expect("write");
        let (app, cx) = open_deep_tree(cx, &repo);
        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("a/other"), cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("file-tree-guide-deep.txt-0").is_some(),
            "precondition: with nothing selected the guide still renders"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("a/b/c/deep.txt"), window, cx);
            app.focus_file_tree(window, cx);
        });
        cx.run_until_parked();

        for selector in [
            "file-tree-guide-deep.txt-0",
            "file-tree-guide-deep.txt-1",
            "file-tree-guide-deep.txt-2",
            "file-tree-guide-elsewhere.txt-0",
            "file-tree-guide-elsewhere.txt-1",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector}: every guide still renders - just always in the one neutral colour"
            );
        }
        for selector in [
            "file-tree-guide-active-deep.txt-0",
            "file-tree-guide-active-deep.txt-1",
            "file-tree-guide-active-deep.txt-2",
            "file-tree-guide-active-elsewhere.txt-0",
            "file-tree-guide-active-elsewhere.txt-1",
        ] {
            assert!(
                cx.debug_bounds(selector).is_none(),
                "{selector}: no guide may ever render under the old accent-colour selector"
            );
        }
    }

    #[gpui::test]
    fn collapsing_removes_the_hidden_rows_guides_too(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let (app, cx) = open_deep_tree(cx, &repo);
        assert!(cx.debug_bounds("file-tree-guide-deep.txt-2").is_some());

        app.update(cx, |app, cx| {
            app.toggle_dir_expanded(repo.path().join("a/b"), cx);
        });
        cx.run_until_parked();

        assert!(cx.debug_bounds("file-tree-row-deep.txt").is_none());
        assert!(
            cx.debug_bounds("file-tree-guide-deep.txt-2").is_none(),
            "a hidden row's guides must be gone with it"
        );
        assert!(
            cx.debug_bounds("file-tree-guide-b-0").is_some(),
            "the still-visible `b` row keeps its own level-0 guide"
        );
    }
}

/// Real interaction coverage for the commit composer (Revision R12 §5,
/// `Self::render_commit_composer`/`Self::render_commit_menu`): a real repo, a real diff, and real
/// `simulate_click`s - matching `virtualization_tests`' and `status_bar_zoom_click_tests`' own
/// `debug_bounds` + `simulate_click` discipline - rather than trusting the composer's own doc
/// comments about what is and isn't wired.
#[cfg(test)]
mod commit_composer_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::process::Command;
    use tempfile::TempDir;

    /// `.output()`, not `.status()` - see `virtualization_tests::git`'s own comment for why.
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

    fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
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
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit_count(dir: &std::path::Path) -> usize {
        git_output(dir, &["rev-list", "--count", "HEAD"])
            .parse()
            .expect("git rev-list --count must print a real integer")
    }

    /// A real feature-branch repo with two files genuinely changed relative to `main`'s
    /// merge-base - the same shape
    /// `virtualization_tests::a_changes_row_far_below_the_viewport_is_never_painted` already
    /// establishes, so this hits `wt_core::diff::DiffBase::Diff` against a real merge-base
    /// rather than `DiffBase::NoBase`'s uncommitted-vs-HEAD fallback (GitHub issue #108).
    fn changes_test_repo() -> TempDir {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "one\ntwo\nthree\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "uno\ndos\ntres\n").expect("write b.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "one\ntwo\nthree\nfour\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "uno\ndos\ntres\ncuatro\n").expect("write b.txt");
        repo
    }

    /// GitHub issue #220's exact shape: a feature branch carrying **one real commit** since its
    /// merge-base with `main` (`committed.txt`, clean on disk - nothing left to stage) plus **one
    /// genuinely uncommitted edit** (`dirty.txt`). `wt_core::diff::diff_against_base` diffs
    /// against that merge-base, so it lists *both* files and `DiffFile` alone cannot tell them
    /// apart - which is precisely what used to make the committed one render an actionable,
    /// unchecked "stage me" checkbox.
    fn repo_with_a_committed_and_a_dirty_file() -> TempDir {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("dirty.txt"), "one\ntwo\n").expect("write dirty.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("committed.txt"), "committed\n")
            .expect("write committed.txt");
        git(repo.path(), &["add", "committed.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "a real commit on the feature branch"],
        );
        std::fs::write(repo.path().join("dirty.txt"), "one\ntwo\nthree\n")
            .expect("modify dirty.txt");
        repo
    }

    fn open_changes_view<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    /// A real click into the message field plus real keystrokes - there is no auto-drafted
    /// fallback any more (see `AdeApp::staged_commit_message`'s own docs), so every test that
    /// needs a real commit to actually happen has to type one first, the same way a real user
    /// would.
    fn type_commit_message(cx: &mut gpui::VisualTestContext, text: &str) {
        let bounds = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.simulate_input(text);
        cx.run_until_parked();
    }

    #[gpui::test]
    fn message_caret_hugs_the_real_text_and_is_vertically_centered(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        let field = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(field.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let empty_caret = cx
            .debug_bounds("commit-composer-message-caret")
            .expect("the caret element must really lay out with an empty message");
        let placeholder = cx
            .debug_bounds("commit-composer-message-empty")
            .expect("the placeholder text must really paint");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "with an empty message, the caret must sit before (at or left of) the \
             placeholder's own start x - got caret {empty_caret:?} vs placeholder \
             {placeholder:?}",
        );

        cx.simulate_input("fix");
        cx.run_until_parked();

        let caret = cx
            .debug_bounds("commit-composer-message-caret")
            .expect("the caret element must really lay out with a typed message");
        let text = cx
            .debug_bounds("commit-composer-message-fix")
            .expect("the real typed message must really paint");
        let field = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        let text_right = text.origin.x + text.size.width;
        assert!(
            caret.origin.x >= text_right,
            "with a typed message, the caret must sit at or after the text's own right edge - \
             got caret {caret:?} vs text {text:?}",
        );
        assert!(
            caret.origin.x - text_right <= px(4.0),
            "the caret must hug the real typed text's right edge, not sit pushed to the \
             field's right edge (the flex_1-on-the-text-div bug) - got caret {caret:?} vs \
             text {text:?}",
        );
        let field_right = field.origin.x + field.size.width;
        assert!(
            field_right - (caret.origin.x + caret.size.width) > px(20.0),
            "for a short message the caret must be well clear of the field's right edge - \
             got caret {caret:?} in field {field:?}",
        );
        let caret_center = caret.origin.y + caret.size.height / 2.0;
        let field_center = field.origin.y + field.size.height / 2.0;
        assert!(
            caret_center >= field_center - px(2.0) && caret_center <= field_center + px(2.0),
            "the caret must be vertically centered in the field - got caret center \
             {caret_center:?} vs field center {field_center:?} (caret {caret:?}, field \
             {field:?})",
        );
    }

    #[gpui::test]
    fn the_composer_reflects_real_staged_count_diffstat_branch_and_message(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        assert_eq!(
            app.read_with(cx, |app, _| app.current_diff().map(|d| d.files.len())),
            Some(2),
            "sanity check: both changed files must really be in the loaded diff, otherwise the \
             assertions below would pass for the wrong reason"
        );

        assert!(
            cx.debug_bounds("commit-composer-progress-0-of-2").is_some(),
            "nothing is staged yet, so the header must show 0 of the real 2 changed files - not \
             a hardcoded or stale count"
        );
        assert!(
            cx.debug_bounds("commit-composer-branch-feature").is_some(),
            "the branch label must show the worktree's real current branch (checked out as \
             `feature`), not a placeholder"
        );
        assert!(
            cx.debug_bounds("commit-composer-message-empty").is_some(),
            "with nothing staged the message box must show the honest empty state, not a \
             fabricated draft"
        );
        assert!(
            cx.debug_bounds("commit-composer-stat-").is_some(),
            "with nothing staged the diffstat must be genuinely empty, not a stale or invented \
             +/- count"
        );

        let a_path = PathBuf::from("a.txt");
        app.update(cx, |app, cx| {
            app.toggle_staged(a_path.clone(), cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("commit-composer-progress-1-of-2").is_some(),
            "staging exactly one of the two real changed files must move the header to a real \
             1 of 2 - a hardcoded header would never move"
        );
        assert!(
            cx.debug_bounds("commit-composer-progress-0-of-2").is_none(),
            "the stale 0-of-2 header must not still be painted alongside the new one"
        );
        assert!(
            cx.debug_bounds("commit-composer-message-empty").is_some(),
            "staging a.txt must not put any text in the message box on its own - there is no \
             auto-drafted fallback (removed per explicit product decision); the user has to type \
             their own message"
        );
        assert!(
            cx.debug_bounds("commit-composer-stat-").is_none(),
            "once something real is staged the diffstat selector must change away from the \
             empty-state string"
        );
        // The real number, not just "it changed": `a.txt` went from `"one\ntwo\nthree\n"` to
        // `"one\ntwo\nthree\nfour\n"` - a single appended line, so a real `git diff` against the
        // merge-base (confirmed independently with a real `git diff` in this same fixture shape
        // while writing this test) reports exactly one added line and zero removed - `+1 −0`
        // (`\u{2212}`, the real minus sign `changes::staged_diff_stats`'s caller formats with,
        // not a plain hyphen).
        assert!(
            cx.debug_bounds("commit-composer-stat-+1 \u{2212}0")
                .is_some(),
            "the diffstat must show the real +1/-0 line count for a.txt's actual change, not a \
             placeholder or a count that merely happens to be non-empty"
        );
    }

    #[gpui::test]
    fn the_primary_button_is_a_genuine_no_op_with_nothing_staged(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let commits_before = commit_count(repo.path());

        let bounds = cx
            .debug_bounds("commit-composer-primary")
            .expect("the primary button must really paint even with nothing staged");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before,
            "a real click on the primary button with nothing staged must never create a real \
             commit"
        );
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_none()),
            "a disabled click must never even start the real commit_staged_files operation"
        );
        // `worktree_history_op_in_flight.is_none()` alone can't tell "never started" apart from
        // "started and already finished" (`Self::commit_staged_files`'s own async task clears it
        // back to `None` on completion too) - `worktree_history_status` is the stronger signal:
        // `commit_staged_files` sets a real "committing…" string *synchronously*, before it ever
        // spawns the background git work, so if the click had reached it at all, this would be
        // `Some` even immediately after `run_until_parked`.
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "a disabled click must never even set the real \"committing…\" status - proof the \
             operation truly never started, not just that it already finished"
        );
    }

    #[gpui::test]
    fn the_primary_button_is_a_genuine_no_op_with_nothing_typed(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.staged_commit_message().is_empty()),
            "premise: something is staged, but no message has been typed"
        );

        let commits_before = commit_count(repo.path());
        let bounds = cx
            .debug_bounds("commit-composer-primary")
            .expect("the primary button must really paint even with no message");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before,
            "a real click on the primary button with something staged but no message must never \
             create a real commit"
        );
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "a disabled click must never even set the real \"committing…\" status"
        );
    }

    #[gpui::test]
    fn clicking_the_message_box_and_typing_really_edits_it(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("commit-composer-message-empty").is_some(),
            "sanity check: staging a.txt puts no text in the box - there is no auto-drafted \
             fallback"
        );

        let bounds = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let (focused, message_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.commit_message_focus_handle.clone())
        });
        assert_eq!(
            focused.as_ref(),
            Some(&message_handle),
            "a real click on the field must really focus it"
        );

        cx.simulate_input("fixes the race, closes #1");
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.commit_message.as_str().to_string()),
            "fixes the race, closes #1",
            "the real keystrokes must land in the field, not vanish"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.staged_commit_message()),
            "fixes the race, closes #1",
            "the edited text must be what the composer would actually commit - the same single \
             source of truth the primary button and the ▾ menu both read"
        );
        assert!(
            cx.debug_bounds("commit-composer-message-fixes the race, closes #1")
                .is_some(),
            "the box must really repaint the user's own typed text"
        );
    }

    #[gpui::test]
    fn typing_before_staging_survives_a_later_stage(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("commit-composer-message-empty").is_some(),
            "sanity check: nothing is staged yet, so the box starts on the empty draft"
        );

        let bounds = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        let message_handle = app.update_in(cx, |app, _window, _cx| {
            app.commit_message_focus_handle.clone()
        });
        assert_eq!(
            app.update_in(cx, |_app, window, cx| window.focused(cx))
                .as_ref(),
            Some(&message_handle),
            "a real click on the empty field must really focus it"
        );

        cx.simulate_input("fixes the race");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.commit_message.as_str().to_string()),
            "fixes the race",
            "sanity check: typing into the empty field works before anything is staged"
        );

        // A real click on the checkbox itself, not a direct `toggle_staged` call - this goes
        // through GPUI's real mouse-down/mouse-up dispatch, which is what a live mousedown-then-
        // blur interaction (were there one) would actually exercise.
        let checkbox = cx
            .debug_bounds("stage-checkbox-a.txt")
            .expect("the staging checkbox must really paint");
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app
                .staged_files
                .contains(&PathBuf::from("a.txt"))),
            "sanity check: the click really staged the file"
        );

        assert_eq!(
            app.read_with(cx, |app, _| app.commit_message.as_str().to_string()),
            "fixes the race",
            "staging a file must not touch the user's own already-typed message"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.staged_commit_message()),
            "fixes the race",
            "and the composer must still read the user's text, not fall back to a fresh draft \
             for the newly-staged file"
        );
        assert_eq!(
            app.update_in(cx, |_app, window, cx| window.focused(cx))
                .as_ref(),
            Some(&message_handle),
            "staging a file must not steal keyboard focus off the message field"
        );

        cx.simulate_input(", closes #2");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.commit_message.as_str().to_string()),
            "fixes the race, closes #2",
            "the field must still be genuinely editable after a stage happened under it - not \
             just displaying stale text but refusing further real keystrokes"
        );
    }

    #[gpui::test]
    fn focusing_the_commit_message_starts_the_real_shared_blink_loop(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        // `on_focus`/`on_blur` only fire while GPUI considers the window itself "active" - see
        // `focusing_the_rail_filter_starts_the_real_shared_blink_loop`'s own docs.
        app.update_in(cx, |_app, window, _cx| window.activate_window());
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.simulate_input("m");
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "a fresh focus must start solid/visible"
        );

        cx.background_executor.advance_clock(
            crate::root::caret_blink::CARET_BLINK_INTERVAL + std::time::Duration::from_millis(50),
        );
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "focusing the commit message field must have started the real, live shared blink \
             task - if `commit_message_focus_handle` were never wired into \
             `AdeApp::wire_caret_blink`, no timer would be running at all and this flag would \
             still be stuck solid"
        );
    }

    #[gpui::test]
    fn a_real_commit_writes_the_users_own_edited_message(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_input("REWRITTEN");
        cx.run_until_parked();

        let primary = cx
            .debug_bounds("commit-composer-primary")
            .expect("the primary button must really paint with something staged and a message");
        cx.simulate_click(primary.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let log = std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(repo.path())
            .output()
            .expect("real git log");
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).trim(),
            "REWRITTEN",
            "the real commit on disk must carry the user's own typed message verbatim"
        );
    }

    #[gpui::test]
    fn focusing_the_message_field_is_not_itself_an_undoable_step(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("commit-composer-message-field")
            .expect("the message field must really paint");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| !app.commit_message.can_undo()),
            "clicking into the field must not itself be a real, undoable edit"
        );

        cx.simulate_input("!");
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.commit_message.can_undo()),
            "sanity check: a real keystroke must be undoable"
        );
    }

    #[gpui::test]
    fn staging_never_writes_anything_into_the_message_box(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        assert_eq!(
            app.read_with(cx, |app, _| app.staged_commit_message()),
            "",
            "sanity check: nothing staged, nothing typed - the box is genuinely empty"
        );

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
            app.toggle_staged(PathBuf::from("b.txt"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.staged_commit_message()),
            "",
            "staging two files must not draft any message at all - the box stays exactly what \
             the user left it as"
        );
        assert!(
            cx.debug_bounds("commit-composer-message-empty").is_some(),
            "and the box really paints the plain empty placeholder, not a derived string"
        );

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
            app.toggle_staged(PathBuf::from("b.txt"), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.staged_commit_message()),
            "",
            "unstaging everything again must not have written anything either"
        );
    }

    #[gpui::test]
    fn clicking_the_primary_button_reaches_the_real_commit_staged_files_action(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let a_path = PathBuf::from("a.txt");
        app.update(cx, |app, cx| {
            app.toggle_staged(a_path.clone(), cx);
        });
        cx.run_until_parked();
        type_commit_message(cx, "update a.txt");

        let commits_before = commit_count(repo.path());

        let bounds = cx
            .debug_bounds("commit-composer-primary")
            .expect("the primary button must really paint once something is staged");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before + 1,
            "a real click on the wired primary button must create exactly one real git commit \
             via wt_core::undo::commit_paths"
        );
        assert!(
            !app.read_with(cx, |app, _| app.staged_files.contains(&a_path)),
            "the just-committed path must be cleared from the real staged set"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.current_diff().map(|d| d.files.len())),
            Some(2),
            "the diff must be reloaded for real after the commit - both files still genuinely \
             differ from the merge-base with main (wt_core::diff diffs the working tree against \
             the merge-base, not the previous commit, per that module's own docs), so the \
             just-committed a.txt correctly stays in the Changes list rather than vanishing"
        );
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_op_in_flight.is_none()),
            "the in-flight flag must clear once the real async commit task finishes"
        );
    }

    #[gpui::test]
    fn clicking_the_menu_toggle_genuinely_opens_and_closes_the_commit_menu(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        assert!(
            !app.read_with(cx, |app, _| app.commit_menu_open),
            "the menu must start closed"
        );
        assert!(
            cx.debug_bounds("commit-menu-scrim").is_none(),
            "an unopened menu's scrim must not be painted at all"
        );

        let toggle_bounds = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "a real click on the toggle must open the real commit_menu_open state"
        );
        assert!(
            cx.debug_bounds("commit-menu-scrim").is_some(),
            "opening the menu must really paint its scrim"
        );
        assert!(
            cx.debug_bounds("commit-menu-popover").is_some(),
            "opening the menu must really paint its popover"
        );

        cx.simulate_click(toggle_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.commit_menu_open),
            "a second click on the toggle must close the menu again"
        );
        assert!(
            cx.debug_bounds("commit-menu-scrim").is_none(),
            "closing the menu must really unpaint its scrim"
        );
    }

    #[gpui::test]
    fn commit_and_push_really_commits_and_reports_the_real_push_failure(cx: &mut TestAppContext) {
        // No `origin` in this fixture, deliberately: the commit half must really happen and the
        // push half must fail *loudly*, with git's own words on the status line. A row that
        // silently swallowed an unreachable remote would be the worst of both worlds.
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();
        type_commit_message(cx, "update a.txt");

        let commits_before = commit_count(repo.path());
        app.update(cx, |app, cx| {
            app.run_commit_menu_action(CommitMenuAction::CommitAndPush, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before + 1,
            "the commit half of `Commit and push` must really create a commit object"
        );
        assert_eq!(
            git_output(repo.path(), &["show", "--format=", "--name-only", "HEAD"]),
            "a.txt",
            "and it must contain exactly the staged path, not the whole worktree"
        );
        let status = app
            .read_with(cx, |app, _| app.worktree_history_status.clone())
            .expect("the action must report what happened");
        assert!(
            status.contains("failed"),
            "with no `origin` configured the push must surface its real failure, not be silently \
             swallowed - got {status:?}"
        );
    }

    #[gpui::test]
    fn commit_all_files_really_commits_the_unstaged_ones_too(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "premise: nothing is staged, so a plain `Commit` could not do this"
        );
        type_commit_message(cx, "commit everything");
        let commits_before = commit_count(repo.path());
        app.update(cx, |app, cx| {
            app.run_commit_menu_action(CommitMenuAction::CommitAllFiles, cx);
        });
        cx.run_until_parked();

        assert_eq!(commit_count(repo.path()), commits_before + 1);
        let committed = git_output(repo.path(), &["show", "--format=", "--name-only", "HEAD"]);
        assert!(
            committed.contains("a.txt") && committed.contains("b.txt"),
            "both changed files must be in the commit - got {committed:?}"
        );
        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain"]),
            "",
            "and the worktree must really be clean afterwards"
        );
    }

    #[gpui::test]
    fn amend_last_commit_really_rewrites_the_tip_without_adding_a_commit(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        git(repo.path(), &["add", "b.txt"]);
        git(repo.path(), &["commit", "-m", "a commit worth amending"]);
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();

        let commits_before = commit_count(repo.path());
        let tip_before = git_output(repo.path(), &["rev-parse", "HEAD"]);
        app.update(cx, |app, cx| {
            app.run_commit_menu_action(CommitMenuAction::AmendLastCommit, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before,
            "an amend must not add a commit - that would be a plain commit wearing the wrong name"
        );
        assert_ne!(
            git_output(repo.path(), &["rev-parse", "HEAD"]),
            tip_before,
            "but it must really rewrite the tip object"
        );
        assert_eq!(
            git_output(repo.path(), &["log", "-1", "--format=%s"]),
            "a commit worth amending",
            "keeping the tip's own message: this row amends, it does not reword"
        );
        assert!(
            git_output(repo.path(), &["show", "--format=", "--name-only", "HEAD"])
                .contains("a.txt"),
            "and the staged file must really be inside the amended tip now"
        );
    }

    #[gpui::test]
    fn stash_staged_files_really_stashes_the_staged_half_only(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("a.txt"), cx);
        });
        cx.run_until_parked();
        type_commit_message(cx, "stash this");

        let commits_before = commit_count(repo.path());
        app.update(cx, |app, cx| {
            app.run_commit_menu_action(CommitMenuAction::StashStaged, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before,
            "stashing is not committing"
        );
        assert!(
            !git_output(repo.path(), &["stash", "list"]).is_empty(),
            "a real stash entry must exist"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("a.txt")).expect("read a.txt"),
            "one\ntwo\nthree\n",
            "the staged edit must be gone from the working tree"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("b.txt")).expect("read b.txt"),
            "uno\ndos\ntres\ncuatro\n",
            "and the unstaged edit must be untouched - the row's hint says `keeps the worktree \
             clean`, not `throws away everything`"
        );
    }

    #[gpui::test]
    fn a_commit_menu_row_with_nothing_staged_is_disabled_and_does_nothing_when_clicked(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "premise: nothing staged"
        );
        type_commit_message(cx, "commit everything");

        let toggle = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        for selector in [
            "commit-menu-row-commit-and-push-disabled",
            "commit-menu-row-amend-last-commit-disabled",
            "commit-menu-row-stash-staged-files-disabled",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} must render as disabled with nothing staged"
            );
        }
        assert!(
            cx.debug_bounds("commit-menu-row-commit-all-files-enabled")
                .is_some(),
            "`Commit all files` stages the rest itself, so it is the one row that is available \
             with nothing staged - and there really are changed files here"
        );

        let commits_before = commit_count(repo.path());
        let disabled = cx
            .debug_bounds("commit-menu-row-stash-staged-files-disabled")
            .expect("the disabled row must really paint");
        cx.simulate_click(disabled.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before,
            "a disabled row must really be a no-op, not merely look like one"
        );
        assert!(
            git_output(repo.path(), &["stash", "list"]).is_empty(),
            "and it must certainly not have stashed anything"
        );
    }

    #[gpui::test]
    fn a_real_click_far_outside_the_composer_closes_the_commit_menu(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let composer = cx
            .debug_bounds("commit-composer")
            .expect("the composer must really paint");
        let toggle = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "premise: the menu must really be open before the click-away below"
        );

        let popover = cx
            .debug_bounds("commit-menu-popover")
            .expect("the popover must really paint while the menu is open");

        // Well up and to the left of both the composer and the popover: the centre pane, hundreds
        // of pixels from anything the old composer-scoped scrim covered.
        let away = gpui::Point {
            x: composer.origin.x / 2.0,
            y: popover.origin.y / 2.0,
        };
        assert!(
            !composer.contains(&away) && !popover.contains(&away),
            "premise: {away:?} must really be outside both the composer {composer:?} and the \
             popover {popover:?}, or this test would be clicking the menu itself"
        );
        cx.simulate_click(away, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.commit_menu_open),
            "a real click far outside the composer must close the commit menu - the popover's \
             scrim is window-wide now, not confined to the composer that used to own it"
        );
        assert!(
            cx.debug_bounds("commit-menu-popover").is_none(),
            "closing must really unpaint the popover, not just flip a flag"
        );
    }

    #[gpui::test]
    fn the_commit_menu_popover_really_paints_anchored_to_the_composer(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let toggle = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let composer = cx
            .debug_bounds("commit-composer")
            .expect("the composer must really paint");
        let popover = cx
            .debug_bounds("commit-menu-popover")
            .expect("the popover must really paint while the menu is open");

        assert_eq!(
            popover.origin.x,
            composer.origin.x + px(12.0),
            "the popover must be inset 12px from the composer's own left edge - a popover \
             anchored off stale or wrongly-scoped bounds would land somewhere else entirely"
        );
        assert_eq!(
            popover.size.width,
            composer.size.width - px(24.0),
            "and inset the same 12px on the right"
        );
        assert!(
            popover.origin.y >= composer.origin.y + composer.size.height,
            "the popover opens *downward* now that GitHub issue #285 pinned the composer above \
             the sections: its top edge {:?} must sit at or below the composer's own bottom edge \
             {:?}. Opening upward from here would open into the panel header and off the top of \
             the window.",
            popover.origin.y,
            composer.origin.y + composer.size.height
        );
        assert!(
            popover.origin.y + popover.size.height > composer.origin.y + composer.size.height,
            "and the four-row popover really extends past the composer's own box - the exact \
             overflow that made a composer-scoped scrim wrong"
        );
        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "sanity check: none of the geometry above may be read from a closed menu"
        );
    }

    #[gpui::test]
    fn leaving_the_changes_view_really_closes_the_commit_menu(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let toggle = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "premise: the menu must really be open before the view switch"
        );

        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Files, window, cx);
        });
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.commit_menu_open),
            "switching away from Changes must close the menu, not latch it"
        );

        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("commit-menu-popover").is_none(),
            "returning to Changes must not resurrect a popover the user never reopened"
        );
    }

    #[gpui::test]
    fn returning_to_an_already_loaded_changes_view_never_blanks_it(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        app.read_with(cx, |app, _| {
            assert!(
                matches!(
                    app.diff_state,
                    crate::code_surface::state::DiffLoadState::Loaded(_)
                ),
                "premise: diff_state must already be Loaded before this test's real assertion"
            );
            assert!(
                app.uncommitted_diff.loaded().is_some(),
                "premise: uncommitted_diff must already be Loaded"
            );
            assert!(
                app.branch_commits.loaded().is_some(),
                "premise: branch_commits must already be Loaded"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Files, window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        });

        // Right here - synchronously, before the refresh this call just dispatched has had any
        // chance to run - every scope must still read as the real, already-loaded data it was a
        // moment ago, not `Loading`/empty.
        app.read_with(cx, |app, _| {
            assert!(
                matches!(
                    app.diff_state,
                    crate::code_surface::state::DiffLoadState::Loaded(_)
                ),
                "diff_state must not be reset to Loading just for re-entering an already-loaded \
                 Changes view - that is the exact empty-then-fills-up flash the live report \
                 named"
            );
            assert!(
                app.uncommitted_diff.loaded().is_some(),
                "uncommitted_diff must keep showing its last real answer through the refresh, \
                 not blank to Loading first"
            );
            assert!(
                app.branch_commits.loaded().is_some(),
                "branch_commits must keep showing its last real answer through the refresh, not \
                 blank to Loading first"
            );
        });

        // The refresh itself still genuinely ran (freshness is preserved, not abandoned) - once
        // it lands, every scope is still Loaded, just from the fresh query rather than the stale
        // one skipped ahead of it.
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(
                app.diff_state,
                crate::code_surface::state::DiffLoadState::Loaded(_)
            ));
            assert!(app.uncommitted_diff.loaded().is_some());
            assert!(app.branch_commits.loaded().is_some());
        });
    }

    #[gpui::test]
    fn switching_worktrees_still_blanks_the_changes_view_synchronously(cx: &mut TestAppContext) {
        let repo_a = changes_test_repo();
        let repo_b = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo_a);

        app.read_with(cx, |app, _| {
            assert!(
                app.uncommitted_diff.loaded().is_some(),
                "premise: the first worktree's diff must already be loaded"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.load_diff(repo_b.path().to_path_buf(), cx);
            let _ = window;
        });

        // Synchronously - before the new worktree's own background reload has run at all - the
        // panel must not still be showing the *previous* worktree's data.
        app.read_with(cx, |app, _| {
            assert!(
                app.uncommitted_diff.loaded().is_none(),
                "a real worktree switch must blank the previous worktree's diff synchronously, \
                 rather than leaving it on screen while the new worktree's own answer loads"
            );
        });

        cx.run_until_parked();
    }

    #[gpui::test]
    fn opening_the_commit_menu_closes_an_already_open_plus_menu(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.plus_menu_open = true;
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("dropdown-menu-row-Git graph").is_some(),
            "premise: the + menu must really be painted before the commit menu opens"
        );

        let toggle = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.commit_menu_open,
                "the click must really open the commit menu"
            );
            assert!(
                !app.plus_menu_open,
                "and must close the + menu - two popovers painted at once is the reported bug"
            );
        });
        assert!(
            cx.debug_bounds("dropdown-menu-row-Git graph").is_none(),
            "the + menu must really stop painting, not merely have its flag cleared"
        );
    }

    #[gpui::test]
    fn opening_the_commit_menu_blocks_a_real_click_on_the_primary_button_underneath(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let a_path = PathBuf::from("a.txt");
        app.update(cx, |app, cx| {
            app.toggle_staged(a_path, cx);
        });
        cx.run_until_parked();

        let primary_bounds = cx
            .debug_bounds("commit-composer-primary")
            .expect("the primary button must really paint once something is staged");
        let toggle_bounds = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "sanity check: the menu must really be open for this to be a real test of the \
             scrim's own click-blocking, not a no-op"
        );

        let commits_before = commit_count(repo.path());

        // The scrim spans the composer's full bounds (`top(0)/left(0)/right(0)/bottom(0)` on a
        // `.relative()` parent) - including the still-visible primary-button row underneath the
        // popover panel, which itself only covers the composer's upper portion
        // (`bottom(px(44.0))`, leaving the button row exposed below it). A real click at the
        // primary button's own screen position, while the menu is open, must be caught by the
        // scrim - not fall through to the real commit action painted underneath it (the
        // `.occlude()`-less-scrim click-through bug class this codebase has hit for real
        // before, e.g. `root::resize::render_resize_handle`'s own `.occlude()`).
        cx.simulate_click(primary_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            commit_count(repo.path()),
            commits_before,
            "a click on the primary button's own screen position, while the scrim covers it, \
             must not reach the real commit action painted underneath"
        );
    }

    #[gpui::test]
    fn the_popover_blocks_a_click_even_where_it_paints_above_the_composers_own_bounds(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let a_path = PathBuf::from("a.txt");
        app.update(cx, |app, cx| {
            app.toggle_staged(a_path, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_none()),
            "sanity check: no diff file is open yet"
        );

        let toggle_bounds = cx
            .debug_bounds("commit-composer-menu-toggle")
            .expect("the ▾ toggle must really paint");
        cx.simulate_click(toggle_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "sanity check: the menu must really be open"
        );

        let popover_bounds = cx
            .debug_bounds("commit-menu-popover")
            .expect("the popover must really paint");
        // The four-row popover paints taller than the short composer it hangs off, so its own
        // top edge genuinely sits above the composer's - outside the scrim's bounds (see
        // `Self::render_commit_menu`'s own docs). A real click right at that top edge is the
        // case most likely to fall through to a real Changes row underneath if the popover
        // relied only on registration-order luck instead of its own `.occlude()`.
        let click_position =
            gpui::point(popover_bounds.center().x, popover_bounds.origin.y + px(2.0));
        cx.simulate_click(click_position, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "a click on the popover's own bounds must not close the menu - that is the scrim's \
             job for a click outside the popover, not the popover's own click"
        );
        assert!(
            app.read_with(cx, |app, _| app.open_change.is_none()),
            "a click on the popover, even where it paints above the composer over the real \
             Changes rows, must never open one of those rows' diffs"
        );
    }

    #[gpui::test]
    fn toggle_staged_really_stages_and_unstages_the_real_git_index(cx: &mut TestAppContext) {
        let repo = changes_test_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        let a_path = PathBuf::from("a.txt");

        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "a.txt"]),
            "M a.txt",
            "sanity check: a.txt must start out modified but genuinely unstaged"
        );

        app.update(cx, |app, cx| {
            app.toggle_staged(a_path.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "a.txt"]),
            "M  a.txt",
            "checking the box must really stage a.txt in the real git index (two-space \
             porcelain form), not just flip an in-memory flag"
        );
        assert!(
            app.read_with(cx, |app, _| app.staged_files.contains(&a_path)),
            "the in-memory staged set must agree with the real index it just changed"
        );

        app.update(cx, |app, cx| {
            app.toggle_staged(a_path.clone(), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "a.txt"]),
            "M a.txt",
            "unchecking the box must really unstage a.txt in the real git index - back to the \
             one-space, working-tree-only porcelain form - not merely forget about it in memory \
             while leaving it staged for real"
        );
        assert!(
            app.read_with(cx, |app, _| !app.staged_files.contains(&a_path)),
            "the in-memory staged set must agree with the real index after the real unstage too"
        );
    }

    #[gpui::test]
    fn a_file_already_staged_in_real_git_before_the_worktree_is_ever_loaded_reads_as_staged(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        // Stages a.txt with a real, direct `git add` *before* the app ever opens this worktree -
        // standing in for a user's own shell or an agent CLI having staged it first.
        git(repo.path(), &["add", "a.txt"]);

        let (app, cx) = open_changes_view(cx, &repo);

        assert_eq!(
            app.read_with(cx, |app, _| app.staged_files.clone()),
            [PathBuf::from("a.txt")].into_iter().collect(),
            "a.txt must read as staged the moment this worktree is loaded - re-derived from the \
             real index `load_diff` just queried, not started at an empty, UI-only set"
        );
        assert!(
            cx.debug_bounds("commit-composer-progress-1-of-2").is_some(),
            "the composer header must reflect the real pre-staged count too, not just the \
             internal `staged_files` set - a stale `0 of 2` here would mean the UI itself never \
             actually saw the real staged state"
        );
    }

    #[gpui::test]
    fn switching_to_a_worktree_with_something_already_staged_in_real_git_shows_it_as_staged(
        cx: &mut TestAppContext,
    ) {
        let repo = changes_test_repo();
        let second_wt = repo.path().join("second-wt");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "second",
                second_wt.to_str().expect("utf8 path"),
            ],
        );
        std::fs::write(second_wt.join("c.txt"), "real change\n").expect("write c.txt");
        git(&second_wt, &["add", "c.txt"]);

        let (app, cx) = open_changes_view(cx, &repo);
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "sanity check: the worktree the window started on has nothing staged"
        );

        let index = app.read_with(cx, |app, _| {
            app.worktrees
                .iter()
                .position(|item| item.path == second_wt)
                .expect("the newly created second worktree must be in the real worktree list")
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(index, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.staged_files.clone()),
            [PathBuf::from("c.txt")].into_iter().collect(),
            "switching into the second worktree must re-derive staged_files from *its* real \
             index (c.txt, staged before this switch ever happened), not carry over the first \
             worktree's empty set or silently stay empty"
        );
    }

    #[gpui::test]
    fn a_committed_file_is_in_the_against_main_section_not_the_uncommitted_one(
        cx: &mut TestAppContext,
    ) {
        let repo = repo_with_a_committed_and_a_dirty_file();
        let (app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("change-row-dirty.txt").is_some(),
            "the genuinely uncommitted file is an Uncommitted row"
        );
        assert!(
            cx.debug_bounds("stage-checkbox-dirty.txt").is_some(),
            "and it keeps its real, actionable staging checkbox"
        );
        assert!(
            cx.debug_bounds("change-row-committed.txt").is_none(),
            "the committed file must not be an Uncommitted row at all - nothing about it is dirty"
        );
        assert!(
            cx.debug_bounds("stage-checkbox-committed.txt").is_none(),
            "so there is no row for it to paint a misleading `stage me` checkbox on - this is \
             issue #220, removed by construction rather than by a per-row condition"
        );

        // ...and it really is counted in the branch-scope section, which starts collapsed - but
        // never as a row of its own, open or not (see `SectionRow::AgainstMainContext`'s own
        // docs: Against main renders no row per file at all).
        assert!(
            cx.debug_bounds("against-main-row-committed.txt").is_none(),
            "premise: Against main starts collapsed, so nothing of it is painted yet"
        );
        app.update(cx, |app, cx| {
            app.toggle_changes_section(sections::ChangesSection::AgainstMain, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("against-main-row-committed.txt").is_none(),
            "expanding Against main must still not paint a row for the committed file - only its \
             one context card"
        );
        let rows = app.update(cx, |app, cx| app.changes_section_rows(cx));
        let against_main_count = rows.iter().find_map(|row| match row {
            sections::SectionRow::Header(header)
                if header.section == sections::ChangesSection::AgainstMain =>
            {
                Some(header.count)
            }
            _ => None,
        });
        assert_eq!(
            against_main_count,
            Some(2),
            "reviewing what the branch would land is exactly what this scope is for - the \
             committed file is still counted (alongside dirty.txt), just not given its own row"
        );
        assert!(
            cx.debug_bounds("stage-checkbox-committed.txt").is_none(),
            "and it offers no checkbox anywhere: `REVISION-2026-08-14.md` §9 box 1, checkboxes \
             exist only in Uncommitted"
        );
    }

    #[gpui::test]
    fn the_composer_counts_only_the_uncommitted_scope_in_its_denominator(cx: &mut TestAppContext) {
        let repo = repo_with_a_committed_and_a_dirty_file();
        let (app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("changes-section-uncommitted-1-open")
                .is_some(),
            "exactly one file is dirty in this checkout, and the section header says so"
        );
        assert!(
            cx.debug_bounds("commit-composer-progress-0-of-1").is_some(),
            "so the composer's denominator is 1 - counting the committed file as outstanding \
             work is exactly the misleading fraction this scope removes"
        );
        assert!(
            cx.debug_bounds("commit-composer-progress-0-of-2").is_none(),
            "the merge-base file count must not leak into a working-tree counter"
        );

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("dirty.txt"), cx);
        });
        cx.run_until_parked();

        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain", "dirty.txt"]),
            "M  dirty.txt",
            "sanity check: that really staged it in the real git index"
        );
        assert!(
            cx.debug_bounds("commit-composer-progress-1-of-1").is_some(),
            "with the only stageable file staged, the composer must read fully staged"
        );
    }

    #[gpui::test]
    fn committing_a_staged_file_moves_it_out_of_uncommitted_and_into_against_main(
        cx: &mut TestAppContext,
    ) {
        let repo = repo_with_a_committed_and_a_dirty_file();
        let (app, cx) = open_changes_view(cx, &repo);

        app.update(cx, |app, cx| {
            app.toggle_staged(PathBuf::from("dirty.txt"), cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("stage-checkbox-dirty.txt").is_some(),
            "sanity check: it is stageable (and now staged) before the commit"
        );
        type_commit_message(cx, "commit the dirty file");

        let commits_before = commit_count(repo.path());
        app.update(cx, |app, cx| {
            app.commit_staged_files(cx);
        });
        cx.run_until_parked();
        assert_eq!(
            commit_count(repo.path()),
            commits_before + 1,
            "sanity check: a real commit must actually have happened"
        );

        assert!(
            cx.debug_bounds("change-row-dirty.txt").is_none(),
            "nothing about it is dirty any more, so it is not an Uncommitted row"
        );
        assert!(
            cx.debug_bounds("stage-checkbox-dirty.txt").is_none(),
            "and it certainly must not still render an unchecked staging checkbox - the exact \
             user-visible report in GitHub issue #220"
        );
        assert!(
            cx.debug_bounds("changes-section-uncommitted-0-open")
                .is_some(),
            "the Uncommitted section is genuinely empty now, and its header says 0"
        );

        app.update(cx, |app, cx| {
            app.toggle_changes_section(sections::ChangesSection::AgainstMain, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("against-main-row-dirty.txt").is_none(),
            "it still differs from `main`, so it is still counted in the branch-scope section - \
             but never as a row of its own, open or not"
        );
        let rows = app.update(cx, |app, cx| app.changes_section_rows(cx));
        let against_main_count = rows.iter().find_map(|row| match row {
            sections::SectionRow::Header(header)
                if header.section == sections::ChangesSection::AgainstMain =>
            {
                Some(header.count)
            }
            _ => None,
        });
        assert_eq!(
            against_main_count,
            Some(2),
            "dirty.txt (just committed) and committed.txt both differ from main now"
        );
    }

    #[gpui::test]
    fn a_deeply_nested_path_does_not_overflow_the_change_row(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("base.txt"), "base\n").expect("write base.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);

        // Twelve real nested directories, each with a long, realistic segment name - the exact
        // shape of path this issue was filed against, not a contrived worst case.
        let nested_dir = repo.path().join(
            [
                "crates",
                "app",
                "src",
                "components",
                "very",
                "deeply",
                "nested",
                "feature",
                "module",
                "implementation",
                "details",
                "internal",
            ]
            .iter()
            .collect::<PathBuf>(),
        );
        std::fs::create_dir_all(&nested_dir).expect("mkdir -p");
        let nested_file = nested_dir.join("a_reasonably_long_source_file_name.rs");
        std::fs::write(&nested_file, "fn main() {}\n").expect("write nested file");

        let (_app, cx) = open_changes_view(cx, &repo);

        let relative = nested_file
            .strip_prefix(repo.path())
            .expect("nested file is under the repo root")
            .display()
            .to_string();
        // `debug_bounds` wants a `&'static str`; these selectors are built at runtime from a
        // real path, so they're leaked once rather than compile-time literals - a real, if
        // deliberately test-only, cost this codebase already accepts elsewhere for the same
        // reason (see `AgentKind`-adjacent `Box::leak` uses).
        let row_selector: &'static str =
            Box::leak(format!("change-row-{relative}").into_boxed_str());
        assert!(
            cx.debug_bounds(row_selector).is_some(),
            "the row for {relative:?} must really render"
        );

        // The row's own outer bounds don't prove anything on their own - a `uniform_list` row
        // slot is a fixed layout box regardless of whether an unclipped `flex_none` child spills
        // out of it, so this measures the directory-prefix element directly instead: it is the
        // one piece of this row `.max_w(px(120.0))` actually bounds, so twelve real nested
        // segments (a directory prefix many times that width, unclipped) must still measure at
        // or under the real cap - not the panel-relative check a whole-row assertion would be.
        let dir_selector: &'static str =
            Box::leak(format!("change-row-dir-{relative}").into_boxed_str());
        let dir_bounds = cx
            .debug_bounds(dir_selector)
            .unwrap_or_else(|| panic!("the directory prefix for {relative:?} must render"));
        assert!(
            dir_bounds.size.width <= px(120.0),
            "the directory prefix must never exceed its real cap - got {:?} for a twelve-\
             segment nested path, which is exactly the case GitHub issue #243 was filed against",
            dir_bounds.size.width
        );
    }
}

/// GitHub issue #285's own acceptance criteria, against a real repository and a real render: four
/// collapsible sections in one scroller, every header's count equal to the rows it really paints,
/// checkboxes in exactly one of them, and Runs summing to Uncommitted.
#[cfg(test)]
mod changes_sections_tests {
    use super::*;
    use crate::provenance::{AgentKey, Author, DiffStat};
    use crate::root::focus::palette_focus_tests;
    use crate::sidebar::sections::{ChangesSection, SectionRow};
    use gpui::TestAppContext;
    use std::process::Command;
    use tempfile::TempDir;

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

    /// A feature branch that genuinely populates all four scopes at once: two commits of its own
    /// (Commits), two still-uncommitted edits (Uncommitted), and therefore four paths that differ
    /// from `main` (Against main).
    fn four_scope_repo() -> TempDir {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("dirty-a.txt"), "one\n").expect("write");
        std::fs::write(repo.path().join("dirty-b.txt"), "one\n").expect("write");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);

        std::fs::write(repo.path().join("done-a.txt"), "committed a\n").expect("write");
        git(repo.path(), &["add", "done-a.txt"]);
        git(repo.path(), &["commit", "-m", "first branch commit"]);
        std::fs::write(repo.path().join("done-b.txt"), "committed b\n").expect("write");
        git(repo.path(), &["add", "done-b.txt"]);
        git(repo.path(), &["commit", "-m", "second branch commit"]);

        std::fs::write(repo.path().join("dirty-a.txt"), "one\ntwo\n").expect("write");
        std::fs::write(repo.path().join("dirty-b.txt"), "one\ntwo\n").expect("write");
        repo
    }

    fn open_changes_view<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    fn open_every_section(app: &gpui::Entity<AdeApp>, cx: &mut gpui::VisualTestContext) {
        app.update(cx, |app, cx| {
            for section in ChangesSection::ORDER {
                if !app.changes_sections.is_open(section) {
                    app.toggle_changes_section(section, cx);
                }
            }
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_panel_is_four_sections_with_runs_and_uncommitted_open_by_default(
        cx: &mut TestAppContext,
    ) {
        let repo = four_scope_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("changes-section-uncommitted-2-open")
                .is_some(),
            "the Uncommitted header must state its real count and start open"
        );
        assert!(
            cx.debug_bounds("changes-section-commits-2-collapsed")
                .is_some(),
            "Commits states a true count while collapsed - that is the whole point of a header \
             count on a closed section"
        );
        assert!(
            cx.debug_bounds("changes-section-against-main-4-collapsed")
                .is_some(),
            "and so does Against main"
        );
        assert!(
            cx.debug_bounds("changes-section-runs-0-open").is_some(),
            "Runs is open by default too, even with no agent having run here yet"
        );

        assert!(
            cx.debug_bounds("change-row-dirty-a.txt").is_some(),
            "an open section paints its rows"
        );
        assert!(
            cx.debug_bounds("against-main-row-done-a.txt").is_none(),
            "a collapsed section paints none of them"
        );
        assert!(
            cx.debug_bounds("changes-commit-").is_none(),
            "nor does Commits"
        );
    }

    #[gpui::test]
    fn the_tab_is_called_changes_and_uncommitted_is_one_of_its_four_sections(
        cx: &mut TestAppContext,
    ) {
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        // Index 1 of the right sidebar's own two-segment toggle - the segment the panel is
        // reached through. Its label is a literal in `render_right_sidebar_toggle`, so what this
        // proves is that the segment exists and is the selected one; the label itself is pinned
        // by the `ChoiceOption::new("Changes")` literal beside it.
        assert!(
            cx.debug_bounds("choice-right-sidebar-toggle-1").is_some(),
            "the panel is reached through the second segment of the Files/Changes toggle"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.right_sidebar_view),
            RightSidebarView::Changes,
            "and that segment selects the Changes view"
        );

        let rows = app.update(cx, |app, cx| app.changes_section_rows(cx));
        let labels: Vec<String> = rows
            .iter()
            .filter_map(|row| match row {
                SectionRow::Header(header) => Some(header.label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "UNCOMMITTED".to_string(),
                "COMMITS".to_string(),
                "AGAINST MAIN".to_string(),
                "RUNS".to_string(),
            ],
            "one panel, four sections, and `Uncommitted` is one of them - never the name of the \
             whole panel"
        );
    }

    #[gpui::test]
    fn every_section_header_count_equals_the_number_of_rows_it_renders(cx: &mut TestAppContext) {
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        open_every_section(&app, cx);

        // The generic form, over the exact row list the renderer consumes: for every header
        // *except Against main*, the number of counted rows between it and the next header is
        // that header's own `count`. Against main is the one exception - it renders no row per
        // file at all (`SectionRow::AgainstMainContext`'s own docs), so its count is checked
        // separately below, against the real file total instead of a rendered-row tally.
        let rows = app.update(cx, |app, cx| app.changes_section_rows(cx));
        let mut current: Option<(ChangesSection, usize)> = None;
        let mut counted = 0usize;
        let mut checked = 0usize;
        let close = |section: ChangesSection, claimed: usize, actual: usize| {
            if section == ChangesSection::AgainstMain {
                return;
            }
            assert_eq!(
                claimed,
                actual,
                "the {} header claims {claimed} rows but the list holds {actual}",
                section.key()
            );
        };
        for row in &rows {
            if let SectionRow::Header(header) = row {
                if let Some((section, claimed)) = current.take() {
                    close(section, claimed, counted);
                    checked += 1;
                }
                current = Some((header.section, header.count));
                counted = 0;
            } else if row.is_counted() {
                counted += 1;
            }
        }
        if let Some((section, claimed)) = current.take() {
            close(section, claimed, counted);
            checked += 1;
        }
        assert_eq!(checked, 4, "all four sections must have been checked");

        let against_main_count = rows.iter().find_map(|row| match row {
            SectionRow::Header(header) if header.section == ChangesSection::AgainstMain => {
                Some(header.count)
            }
            _ => None,
        });
        assert_eq!(
            against_main_count,
            Some(4),
            "against main's count is the real file total (dirty-a, dirty-b, done-a, done-b), \
             not a tally of rows it never renders"
        );

        for selector in ["change-row-dirty-a.txt", "change-row-dirty-b.txt"] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "{selector} must really paint once its section is open"
            );
        }
    }

    #[gpui::test]
    fn checkboxes_exist_only_in_the_uncommitted_section(cx: &mut TestAppContext) {
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        open_every_section(&app, cx);

        assert!(
            cx.debug_bounds("stage-checkbox-dirty-a.txt").is_some(),
            "the Uncommitted section stages"
        );
        assert!(
            cx.debug_bounds("against-main-row-dirty-a.txt").is_none(),
            "Against main renders no row per file at all (`SectionRow::AgainstMainContext`'s own \
             docs), so there is no second row here for a stray checkbox to double up on"
        );
        // The structural guarantee is `ChangesSection::has_checkboxes`, asserted directly in
        // `crate::sidebar::sections`' own tests; this is its rendered half.
        let rows = app.update(cx, |app, cx| app.changes_section_rows(cx));
        let sections_with_files: Vec<ChangesSection> = rows
            .iter()
            .filter_map(|row| match row {
                SectionRow::UncommittedFile(_) => Some(ChangesSection::Uncommitted),
                SectionRow::Commit(_) => Some(ChangesSection::Commits),
                SectionRow::Run(_) => Some(ChangesSection::Runs),
                _ => None,
            })
            .collect();
        assert!(
            sections_with_files.contains(&ChangesSection::Commits),
            "premise: the other file-bearing section really does have rows here"
        );
        assert_eq!(
            sections_with_files
                .iter()
                .filter(|section| section.has_checkboxes())
                .count(),
            2,
            "only the two Uncommitted rows may stage - every other row in the panel is read-only"
        );
    }

    #[gpui::test]
    fn collapsing_a_section_unpaints_its_rows_and_keeps_its_count(cx: &mut TestAppContext) {
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        assert!(cx.debug_bounds("change-row-dirty-a.txt").is_some());

        app.update(cx, |app, cx| {
            app.toggle_changes_section(ChangesSection::Uncommitted, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("change-row-dirty-a.txt").is_none(),
            "collapsing must really unpaint the rows, not merely hide them"
        );
        assert!(
            cx.debug_bounds("changes-section-uncommitted-2-collapsed")
                .is_some(),
            "but the header must keep stating the true count - triage needs to see that there \
             *are* uncommitted changes without operating a control"
        );
        assert!(
            cx.debug_bounds("changes-section-runs-0-open").is_some(),
            "and collapsing one section must not move another"
        );
    }

    #[gpui::test]
    fn the_against_main_section_renders_no_action_buttons_and_no_file_rows(
        cx: &mut TestAppContext,
    ) {
        // The product decision recorded on GitHub issue #285, overriding
        // `REVISION-2026-08-14.md` §1 rule 3 and `STAGE-A-CHANGELOG.md` §4e: merging is the git
        // graph's job and worktree removal already has its entry point on the rail, so this
        // section is read-only.
        //
        // And, separately: no row per committed file either, reported directly ("commited files
        // should not appear on the changes tab under against master") and confirmed against
        // `Jerry.dc.html` line 1422's own `baseRows` - a synthetic one-entry array, never a
        // per-file loop. Against main's only real body row is its context card.
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        open_every_section(&app, cx);

        let rows = app.update(cx, |app, cx| app.changes_section_rows(cx));
        let against_main: Vec<&SectionRow> = rows
            .iter()
            .skip_while(|row| !matches!(row, SectionRow::Header(header) if header.section == ChangesSection::AgainstMain))
            .skip(1)
            .take_while(|row| !matches!(row, SectionRow::Header(_)))
            .collect();
        assert!(
            !against_main.is_empty(),
            "premise: the section really has a body here"
        );
        for row in &against_main {
            assert!(
                matches!(
                    row,
                    SectionRow::AgainstMainContext { .. } | SectionRow::Note { .. }
                ),
                "the Against-main section may only hold its context card and its own notes - no \
                 action rows, and no row per file: {row:?}"
            );
        }
        assert!(
            against_main
                .iter()
                .any(|row| matches!(row, SectionRow::AgainstMainContext { .. })),
            "it does still carry the read-only commit context"
        );
        assert!(
            cx.debug_bounds("against-main-row-done-a.txt").is_none(),
            "and neither committed file painted a row of its own"
        );
        assert!(cx.debug_bounds("against-main-row-done-b.txt").is_none());
    }

    #[gpui::test]
    fn the_runs_header_matches_the_uncommitted_header_when_one_agent_wrote_everything(
        cx: &mut TestAppContext,
    ) {
        // `STAGE-A-CHANGELOG.md` §3's own verification of the mock, as a live render: "Runs
        // `+319 −145` and Uncommitted `+319 −145` agree exactly." The provenance is recorded
        // through the store's real `PreToolUse`/write/`PostToolUse` door, exactly as the hook
        // layer drives it - no fabricated attribution.
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("agent.txt"), "one\n").expect("write");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);

        let (app, cx) = open_changes_view(cx, &repo);
        let agent_id = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("the real startup agent");
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(agent_id, ProcessKind::claude());
        });

        let key = app.read_with(cx, |app, _| {
            let agent = app
                .agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .expect("agent");
            AgentKey::new(crate::review::state::baseline_key(
                &agent.cwd,
                crate::work_surface::agents::AgentKind::Claude,
                agent.spawned_at_unix,
            ))
        });
        let file = repo.path().join("agent.txt");
        app.update(cx, |app, _cx| {
            app.line_provenance.begin_agent_edit(repo.path(), &file);
        });
        std::fs::write(&file, "one\ntwo\nthree\n").expect("agent writes");
        app.update_in(cx, |app, window, cx| {
            app.line_provenance
                .record_agent_edit(repo.path(), &file, &key);
            app.load_diff(repo.path().to_path_buf(), cx);
            let _ = window;
        });
        cx.run_until_parked();

        let (runs, uncommitted) = app.read_with(cx, |app, _| {
            (
                app.uncommitted_change_set
                    .split()
                    .get(&Author::Agent(key.clone()))
                    .copied()
                    .unwrap_or_default(),
                app.uncommitted_change_set.total(),
            )
        });
        assert_eq!(
            runs,
            DiffStat::new(2, 0),
            "sanity: the agent really added two lines, as git sees it"
        );
        assert_eq!(
            runs, uncommitted,
            "every uncommitted line here is this agent's, so the Runs total is the Uncommitted \
             total exactly - by construction, since both are read off one partition"
        );

        assert!(
            cx.debug_bounds("changes-section-runs-stat-+2-\u{2212}0")
                .is_some(),
            "the Runs header must paint the agent's own share"
        );
        assert!(
            cx.debug_bounds("changes-section-uncommitted-stat-+2-\u{2212}0")
                .is_some(),
            "and the Uncommitted header must paint the identical figure"
        );
        assert!(
            cx.debug_bounds("changes-section-runs-1-open").is_some(),
            "one agent has run here, so the Runs section has exactly one row"
        );
        assert!(
            cx.debug_bounds("changes-run-0").is_some()
                || cx.debug_bounds("changes-run-1").is_some(),
            "and that row must really paint"
        );
    }

    #[gpui::test]
    fn a_live_runs_row_states_that_it_is_running_and_switching_focus_changes_no_count(
        cx: &mut TestAppContext,
    ) {
        // Audit I2: the header count equals the rendered row count, and switching agent focus
        // changes neither. Before the union, the header read one agent's set while the rows read
        // another's - two sources able to disagree on screen.
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        let first = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("the real startup agent");
        app.update(cx, |app, _cx| {
            app.agents.set_kind_for_test(first, ProcessKind::claude());
        });
        let second = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::claude(),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        // `Agents::spawn` notifies its own new pane entity, not the root, so the right sidebar is
        // not repainted by the spawn alone - the same nudge every other test in this file that
        // spawns mid-test needs.
        app.update(cx, |_app, cx| cx.notify());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("changes-section-runs-2-open").is_some(),
            "two real agent sessions in this worktree are two runs"
        );
        assert!(
            cx.debug_bounds("changes-section-uncommitted-2-open")
                .is_some(),
            "and the Uncommitted count is its own, unrelated, real number"
        );

        let rows = app.update(cx, |app, cx| app.changes_run_rows(cx));
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter().all(|row| row.live),
            "both processes are genuinely alive, so both rows must say so"
        );
        assert!(
            rows.iter().all(|row| row.meta.contains("running")),
            "a live run's meta line reads `running`: {:?}",
            rows.iter().map(|row| row.meta.clone()).collect::<Vec<_>>()
        );

        app.update_in(cx, |app, window, cx| {
            app.select_agent(second, window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("changes-section-uncommitted-2-open")
                .is_some(),
            "the Uncommitted count is a fact about the worktree, not about which agent is focused"
        );
        assert!(
            cx.debug_bounds("changes-section-runs-2-open").is_some(),
            "and so is the Runs count"
        );
        assert!(
            cx.debug_bounds("change-row-dirty-a.txt").is_some(),
            "and the rows themselves must not change under a focus switch either"
        );

        app.update_in(cx, |app, window, cx| {
            app.select_agent(first, window, cx);
        });
        cx.run_until_parked();
        assert!(cx.debug_bounds("changes-section-runs-2-open").is_some());
    }

    #[gpui::test]
    fn a_live_run_and_an_ended_run_render_side_by_side(cx: &mut TestAppContext) {
        // The mock's sad path (`STAGE-A-SELFCHECK.md`): "a live run and a frozen run render side
        // by side in the Runs section […] both rows verifiable in one screen." The ended run here
        // is a genuinely exited process, not a flag.
        let repo = four_scope_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        let live = app
            .read_with(cx, |app, _| app.agents.active_id())
            .expect("the real startup agent");
        app.update(cx, |app, _cx| {
            app.agents.set_kind_for_test(live, ProcessKind::claude());
        });
        // A real process that really exits immediately: `/bin/false` returns 1 and is gone.
        // Spawned as a `Shell` because `shell_override` only applies to that kind
        // (`Agents::spawn_inner` builds an agent's `TerminalSpec` from the CLI itself), then
        // relabelled - the same real-process-plus-relabel the title bar's own live tests use, so
        // what this asserts about is a genuinely exited child, not a flag.
        let ended = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                Some("/bin/false"),
                None,
                window,
                cx,
            )
        });
        app.update(cx, |app, _cx| {
            app.agents.set_kind_for_test(ended, ProcessKind::claude());
        });
        cx.run_until_parked();
        // A real OS process really has to exit, which takes real wall-clock time - so this waits
        // on the genuine `is_running()` transition rather than assuming it, and gives up loudly
        // rather than asserting against a process that never died.
        let mut exited = false;
        for _ in 0..200 {
            app.update(cx, |_app, cx| cx.notify());
            cx.run_until_parked();
            if app.read_with(cx, |app, cx| {
                app.agents
                    .iter()
                    .find(|agent| agent.id == ended)
                    .is_some_and(|agent| !agent.pane.read(cx).is_running())
            }) {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            // The pane notices its child's real pty EOF on its own poll timer, and this context's
            // executor is deterministic - so real time has to be given to the child *and* the
            // clock has to be advanced for the poll that observes it.
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(50));
        }
        assert!(
            exited,
            "premise: the `/bin/false` agent must really have exited, or this test would be \
             asserting about a live process"
        );

        let rows = app.update(cx, |app, cx| app.changes_run_rows(cx));
        assert_eq!(rows.len(), 2, "both runs are in the section at once");
        let live_row = rows
            .iter()
            .find(|row| row.agent_id == live)
            .expect("the live run's row");
        let ended_row = rows
            .iter()
            .find(|row| row.agent_id == ended)
            .expect("the ended run's row");

        assert!(live_row.live, "the still-alive process is a live run");
        assert!(live_row.meta.contains("running"), "got {:?}", live_row.meta);
        assert!(
            !ended_row.live,
            "the real, exited process is an ended run - this is a genuine `is_running() == false`, \
             not a test flag"
        );
        assert!(
            ended_row.meta.contains("ended"),
            "an ended run's meta says `ended`: got {:?}",
            ended_row.meta
        );
        assert_ne!(
            live_row.meta_color(),
            ended_row.meta_color(),
            "warm while it is still moving, neutral once it has ended - that colour split is the \
             only thing carrying the state on the row itself (STAGE-A-CHANGELOG.md §4l)"
        );
        assert!(
            cx.debug_bounds("changes-section-runs-2-open").is_some(),
            "and both rows are on one screen, under one header stating two"
        );
    }
}

/// GitHub issue #286 - the change row's three rev-6 channels, each asserted against a real,
/// painted row: git's own status letter (`STAGE-A-CHANGELOG.md` §4j), the filename's own
/// seen-state (§4i), and the floating hover-action bar with its two-step discard (§4i).
#[cfg(test)]
mod change_row_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::sidebar::changes::StatusLetter;
    use gpui::TestAppContext;
    use std::process::Command;
    use tempfile::TempDir;

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

    /// One worktree holding one of each real git status at once: `added.txt` is brand new,
    /// `modified.txt` has an edit, `deleted.txt` is gone. §4j's whole point is that all three -
    /// not only the two exceptions - carry a mark.
    fn mixed_status_repo() -> TempDir {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("modified.txt"), "one\n").expect("write");
        std::fs::write(repo.path().join("deleted.txt"), "gone\n").expect("write");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);

        std::fs::write(repo.path().join("modified.txt"), "one\ntwo\n").expect("write");
        std::fs::remove_file(repo.path().join("deleted.txt")).expect("delete");
        std::fs::write(repo.path().join("added.txt"), "brand new\n").expect("write");
        repo
    }

    fn open_changes_view<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        });
        cx.run_until_parked();
        (app, cx)
    }

    #[gpui::test]
    fn every_row_carries_gits_own_status_letter_including_the_modified_one(
        cx: &mut TestAppContext,
    ) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("change-row-added.txt-status-A").is_some(),
            "a brand-new file is `A`"
        );
        assert!(
            cx.debug_bounds("change-row-modified.txt-status-M")
                .is_some(),
            "and a modified one is `M` - the row that used to carry nothing at all"
        );
        assert!(
            cx.debug_bounds("change-row-deleted.txt-status-D").is_some(),
            "and a deleted one is `D`"
        );
    }

    #[gpui::test]
    fn the_letter_column_is_fixed_width_and_ahead_of_the_name(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        let added = cx
            .debug_bounds("change-row-added.txt-status-A")
            .expect("the `A` column");
        let deleted = cx
            .debug_bounds("change-row-deleted.txt-status-D")
            .expect("the `D` column");
        assert_eq!(
            added.size.width, deleted.size.width,
            "one shared optical box, not one width per letter"
        );
        assert_eq!(
            added.origin.x, deleted.origin.x,
            "and one shared x, which is what makes the names below line up"
        );

        let name = cx
            .debug_bounds("change-row-added.txt-name-unseen")
            .expect("the filename cell");
        assert!(
            added.origin.x + added.size.width <= name.origin.x,
            "the letter sits *ahead* of the name, not after it where the pill used to be: letter \
             ends at {:?}, name starts at {:?}",
            added.origin.x + added.size.width,
            name.origin.x
        );
    }

    #[gpui::test]
    fn the_file_header_above_the_diff_carries_the_same_letter(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(PathBuf::from("modified.txt")),
            "premise: the click really opened the file"
        );
        assert!(
            cx.debug_bounds("code-surface-status-M").is_some(),
            "the toolbar above the diff states what git did to this file, in the same column the \
             word pill used to occupy"
        );
    }

    #[gpui::test]
    fn opening_a_file_moves_its_name_from_unseen_to_seen(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("change-row-modified.txt-name-unseen")
                .is_some(),
            "nothing has been looked at yet"
        );

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("change-row-modified.txt-name-seen")
                .is_some(),
            "opening a file marks it seen - that is what the word means (§4i)"
        );
        assert!(
            cx.debug_bounds("change-row-added.txt-name-unseen")
                .is_some(),
            "and only that file: seen is per path, not a panel-wide flag"
        );
    }

    #[gpui::test]
    fn seen_and_staged_are_two_independent_maps(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx
            .debug_bounds("change-row-modified.txt-name-seen")
            .is_some());
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "reviewing must never stage (REVISION-2026-08-14.md §1 rule 2)"
        );

        let checkbox = cx
            .debug_bounds("stage-checkbox-added.txt")
            .expect("the added row's checkbox");
        cx.simulate_click(checkbox.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app
                .staged_files
                .contains(&PathBuf::from("added.txt"))),
            "premise: the checkbox really staged it"
        );
        assert!(
            cx.debug_bounds("change-row-added.txt-name-unseen")
                .is_some(),
            "and the name is untouched - the checkbox owns staged, the name owns seen"
        );
    }

    #[gpui::test]
    fn v_unmarks_a_seen_file_and_marks_an_unseen_one(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(cx
            .debug_bounds("change-row-modified.txt-name-seen")
            .is_some());

        app.update_in(cx, |app, window, cx| {
            app.handle_toggle_change_seen_action(&crate::root::ToggleChangeSeen, window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("change-row-modified.txt-name-unseen")
                .is_some(),
            "`V` unmarks (§4i)"
        );

        app.update_in(cx, |app, window, cx| {
            app.handle_toggle_change_seen_action(&crate::root::ToggleChangeSeen, window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("change-row-modified.txt-name-seen")
                .is_some(),
            "and marks again - the legend `V mark seen` has to be true as well"
        );
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "and it never touches the staged set on the way through"
        );
    }

    #[test]
    fn the_changes_footer_only_advertises_a_really_registered_binding() {
        let bindings = crate::default_key_bindings();
        let binding = bindings
            .iter()
            .find(|binding| binding.action().name() == "app::ToggleChangeSeen")
            .expect("`ToggleChangeSeen` must have a real registered binding");
        let keystrokes = binding.keystrokes();
        assert_eq!(keystrokes.len(), 1, "one plain keystroke, no chord");
        let keystroke = &keystrokes[0];
        assert_eq!(
            keystroke.key(),
            CHANGES_SEEN_SPEC,
            "the footer prints the keycap for {CHANGES_SEEN_SPEC:?}, so that is what has to be \
             bound"
        );
        let modifiers = keystroke.modifiers();
        assert!(
            !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.shift,
            "a bare `V`, exactly as §4i's legend prints it - a binding that silently gained a \
             modifier would leave the keycap advertising a keystroke nobody can trigger"
        );
    }

    #[gpui::test]
    fn the_hover_bar_is_absent_until_the_row_is_hovered(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("change-row-actions-modified.txt").is_none(),
            "nothing floats over an unhovered row"
        );

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("change-row-actions-modified.txt").is_some(),
            "hovering the row reveals it"
        );
        assert!(
            cx.debug_bounds("change-row-actions-added.txt").is_none(),
            "and only over the row the pointer is really on"
        );
    }

    #[gpui::test]
    fn the_hover_bar_is_two_icons_floating_above_the_rows_top_edge(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();

        let bar = cx
            .debug_bounds("change-row-actions-modified.txt")
            .expect("the floating bar");
        assert!(
            bar.origin.y < row.origin.y,
            "it straddles the row's top edge - bar at {:?}, row at {:?}",
            bar.origin.y,
            row.origin.y
        );
        assert!(
            bar.origin.x + bar.size.width <= row.origin.x + row.size.width,
            "and stays inside the row's right edge, where §4i right-aligns it"
        );

        assert!(cx
            .debug_bounds("change-row-open-in-editor-modified.txt")
            .is_some());
        assert!(cx.debug_bounds("change-row-discard-modified.txt").is_some());
    }

    #[gpui::test]
    fn the_first_discard_click_only_arms_the_confirm_and_changes_nothing(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let before = std::fs::read_to_string(repo.path().join("modified.txt")).expect("read");
        let (app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();

        let discard = cx
            .debug_bounds("change-row-discard-modified.txt")
            .expect("the discard icon");
        cx.simulate_click(discard.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("change-row-discard-confirm-modified.txt")
                .is_some(),
            "the icon is replaced by the red `Discard?` pill"
        );
        assert!(
            cx.debug_bounds("change-row-discard-modified.txt").is_none(),
            "and the plain icon is gone - one control, one state, not both at once"
        );
        assert_eq!(
            std::fs::read_to_string(repo.path().join("modified.txt")).expect("read"),
            before,
            "arming must not touch the file: this is the one irreversible action in the panel"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.change_row_discard_armed.clone()),
            Some(PathBuf::from("modified.txt"))
        );
    }

    #[gpui::test]
    fn the_second_discard_click_really_throws_the_files_changes_away(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
        let discard = cx
            .debug_bounds("change-row-discard-modified.txt")
            .expect("the discard icon");
        cx.simulate_click(discard.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let confirm = cx
            .debug_bounds("change-row-discard-confirm-modified.txt")
            .expect("the armed pill");
        cx.simulate_click(confirm.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(repo.path().join("modified.txt")).expect("read"),
            "one\n",
            "the file is back to exactly what HEAD has - a real `git checkout HEAD -- <path>`, \
             not a UI-only flag"
        );
        assert!(
            std::fs::read_to_string(repo.path().join("added.txt")).is_ok(),
            "and every other dirty path is untouched"
        );
    }

    #[gpui::test]
    fn leaving_the_row_cancels_an_armed_discard(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
        let discard = cx
            .debug_bounds("change-row-discard-modified.txt")
            .expect("the discard icon");
        cx.simulate_click(discard.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.change_row_discard_armed.is_some()));

        cx.simulate_mouse_move(
            gpui::Point::new(px(4.0), px(4.0)),
            None,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.change_row_discard_armed.clone()),
            None,
            "the confirm is disarmed the moment the pointer leaves"
        );
        assert!(
            cx.debug_bounds("change-row-actions-modified.txt").is_none(),
            "and the bar goes with it"
        );
    }

    #[gpui::test]
    fn moving_onto_the_bars_overhang_keeps_it_open(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_mouse_move(row.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();
        let bar = cx
            .debug_bounds("change-row-actions-modified.txt")
            .expect("the floating bar");

        let overhang =
            gpui::Point::new(bar.origin.x + bar.size.width / 2.0, bar.origin.y + px(2.0));
        assert!(
            overhang.y < row.origin.y,
            "premise: this point really is outside the row's own bounds"
        );
        cx.simulate_mouse_move(overhang, None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("change-row-actions-modified.txt").is_some(),
            "the bar survives the pointer reaching for its own icons"
        );
    }

    #[gpui::test]
    fn the_section_header_counts_seen_off_the_same_map_the_rows_read(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("changes-section-uncommitted-0-of-3-seen")
                .is_some(),
            "three dirty files, none looked at yet"
        );

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("changes-section-uncommitted-1-of-3-seen")
                .is_some(),
            "opening one file moves the header's own counter, because it is the same map"
        );
    }

    #[gpui::test]
    fn the_footer_shows_the_v_keycap_only_while_the_binding_is_live(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("changes-footer-seen-hint").is_none(),
            "with no file open, `V` would do nothing - so the strip advertises nothing"
        );

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("changes-footer-seen-hint").is_some(),
            "with a real file open, the keycap appears and really does something"
        );
    }

    #[gpui::test]
    fn the_footer_shows_the_space_keycap_only_while_the_binding_is_live(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        assert!(
            cx.debug_bounds("changes-footer-stage-hint").is_none(),
            "with no file open, `space` would do nothing - so the strip advertises nothing"
        );

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("changes-footer-stage-hint").is_some(),
            "with a real uncommitted file open, the keycap appears and really does something"
        );
    }

    #[gpui::test]
    fn the_footers_first_child_is_the_stage_keycap_at_the_bands_own_left_padding(
        cx: &mut TestAppContext,
    ) {
        let repo = mixed_status_repo();
        let (_app, cx) = open_changes_view(cx, &repo);

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        let footer = cx.debug_bounds("changes-footer").expect("the footer band");
        let stage = cx
            .debug_bounds("changes-footer-stage-hint")
            .expect("the `space stage` hint");
        let seen = cx
            .debug_bounds("changes-footer-seen-hint")
            .expect("the `V mark seen` hint");

        assert!(
            (stage.origin.x - (footer.origin.x + px(12.0))).abs() < px(1.0),
            "the first hint must start at the band's own 12px padding - it started at {:?} \
             against a band at {:?}, which means something (prose) is sitting ahead of it",
            stage.origin.x,
            footer.origin.x
        );
        assert!(
            seen.origin.x > stage.origin.x,
            "and `V mark seen` follows `space stage`, the mock's own order"
        );
    }

    #[gpui::test]
    fn space_stages_and_unstages_the_open_change_through_the_real_key_binding(
        cx: &mut TestAppContext,
    ) {
        let repo = mixed_status_repo();
        let (app, cx) = open_changes_view(cx, &repo);
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        let row = cx
            .debug_bounds("change-row-modified.txt")
            .expect("the modified row");
        cx.simulate_click(row.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(PathBuf::from("modified.txt")),
            "premise: the click really opened that file's diff"
        );
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "premise: opening a file never stages it (REVISION-2026-08-14.md §1 rule 2)"
        );

        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app
                .staged_files
                .contains(&PathBuf::from("modified.txt"))),
            "a real `space` keystroke stages the open row - `Jerry.dc.html`'s `space stage`"
        );
        assert!(
            cx.debug_bounds("stage-checkbox-modified.txt").is_some(),
            "and it acted on the row that really carries a checkbox - staging is the Uncommitted \
             section's affordance"
        );

        cx.simulate_keystrokes("space");
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "and it toggles back off, the same way the checkbox does"
        );
        assert!(
            cx.debug_bounds("change-row-modified.txt-name-seen")
                .is_some(),
            "throughout, `space` never touches the name's seen-state - the checkbox owns staged, \
             the name owns seen (audit I3)"
        );
    }

    #[gpui::test]
    fn space_does_nothing_for_a_file_with_no_uncommitted_row(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("dirty.txt"), "one\n").expect("write");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("committed.txt"), "committed\n").expect("write");
        git(repo.path(), &["add", "committed.txt"]);
        git(
            repo.path(),
            &["commit", "-m", "a real commit on the feature branch"],
        );
        std::fs::write(repo.path().join("dirty.txt"), "one\ntwo\n").expect("modify");

        let (app, cx) = open_changes_view(cx, &repo);
        assert!(
            cx.debug_bounds("change-row-dirty.txt").is_some(),
            "premise: the genuinely uncommitted file really is an Uncommitted row"
        );
        assert!(
            cx.debug_bounds("stage-checkbox-committed.txt").is_none(),
            "premise: the committed-clean file has no checkbox anywhere (issue #220)"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("committed.txt"), window, cx);
        });
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.change_stage_toggle_live()),
            "so the hint is hidden - nothing is advertised"
        );
        assert!(
            cx.debug_bounds("changes-footer-stage-hint").is_none(),
            "and really not painted, not just reported hidden"
        );

        app.update_in(cx, |app, window, cx| {
            app.handle_toggle_change_staged_action(&crate::root::ToggleChangeStaged, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.staged_files.is_empty()),
            "and the keystroke itself is inert too - no invented entry for a file with no \
             uncommitted delta"
        );
    }

    #[test]
    fn the_changes_footers_space_hint_only_advertises_a_really_registered_binding() {
        let bindings = crate::default_key_bindings();
        let find = |name: &str| {
            bindings
                .iter()
                .find(|binding| binding.action().name() == name)
                .unwrap_or_else(|| panic!("`{name}` must have a real registered binding"))
        };
        let stage = find("app::ToggleChangeStaged");
        let seen = find("app::ToggleChangeSeen");

        let keystrokes = stage.keystrokes();
        assert_eq!(keystrokes.len(), 1, "one plain keystroke, no chord");
        let keystroke = &keystrokes[0];
        assert_eq!(
            keystroke.key(),
            CHANGES_STAGE_SPEC,
            "the footer prints the keycap for {CHANGES_STAGE_SPEC:?}, so that is what has to be \
             bound"
        );
        let modifiers = keystroke.modifiers();
        assert!(
            !modifiers.control && !modifiers.alt && !modifiers.platform && !modifiers.shift,
            "a bare `space`, exactly as `Jerry.dc.html`'s `changesHints` prints it"
        );
        let predicate = |binding: &gpui::KeyBinding| {
            binding
                .predicate()
                .map(|predicate| predicate.to_string())
                .expect("a real, scoped predicate - never global")
        };
        assert_eq!(
            predicate(stage),
            "diff && !file-editor && !text-input",
            "`!file-editor` keeps a typed space out of this binding's hands in the File view, and \
             `!text-input` does the same for GitHub issue #288's pinned review-note card - a real \
             text input *inside* the `\"diff\"` node that `\"file-editor\"` does not cover, where \
             a bare space would otherwise stage the file instead of separating two words"
        );
        assert_eq!(
            predicate(stage),
            predicate(seen),
            "and both hints in the one strip name the same real surface"
        );
    }

    #[gpui::test]
    fn the_rows_letter_is_the_status_the_real_diff_reports(cx: &mut TestAppContext) {
        let repo = mixed_status_repo();
        let (app, cx) = open_changes_view(cx, &repo);

        let letters = app.read_with(cx, |app, _| {
            app.uncommitted_change_set
                .entries()
                .iter()
                .map(|entry| {
                    (
                        entry.path.display().to_string(),
                        changes::status_letter(entry.status),
                    )
                })
                .collect::<Vec<_>>()
        });
        assert!(letters.contains(&("added.txt".to_string(), StatusLetter::Added)));
        assert!(letters.contains(&("modified.txt".to_string(), StatusLetter::Modified)));
        assert!(letters.contains(&("deleted.txt".to_string(), StatusLetter::Deleted)));
    }
}
