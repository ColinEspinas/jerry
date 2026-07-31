//! Opening, activating and closing Surface C's file and diff tabs, plus the background
//! loads behind them (`wt_core::diff` for a diff, `std::fs::read_to_string` +
//! tree-sitter for a file). No drawing happens here - see the sibling `render`/
//! `diff_view`/`file_view` modules for that.

use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

impl AdeApp {
    /// Loads (or reloads) the diff of `root` against its detected base branch. Runs on
    /// `cx.background_executor()` since `diff_against_base` does blocking I/O (gix reads plus a
    /// spawned `git diff` process) and must not run on the GPUI foreground thread.
    pub(crate) fn load_diff(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.diff_root = root.clone();
        self.diff_state = DiffLoadState::Loading;
        self.diff_totals = None;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move {
                        // Fold the +n/-n totals here, off the UI thread, rather than
                        // recomputing them on every render.
                        wt_core::diff::diff_against_base(&root).map(|base| {
                            let totals = match &base {
                                DiffBase::Diff(diff) => Some(diff.files.iter().fold(
                                    (0u32, 0u32),
                                    |(add, del), file| {
                                        let (file_add, file_del) = changes::diff_file_stats(file);
                                        (add + file_add, del + file_del)
                                    },
                                )),
                                DiffBase::NoBaseFound | DiffBase::OnDefaultBranch { .. } => None,
                            };
                            (base, totals)
                        })
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((base, totals)) => {
                        this.diff_state = DiffLoadState::Loaded(base);
                        this.diff_totals = totals;
                    }
                    Err(err) => {
                        this.diff_state = DiffLoadState::Error(err.to_string());
                        this.diff_totals = None;
                    }
                }
                // The reloaded diff may have changed whether `open_change`'s path has a
                // `DiffFile`, so refresh the cache immediately rather than leaving it stale.
                this.refresh_open_diff_file_cache();
                // The palette's file-candidate list also carries diff marks; rebuild it too.
                this.rebuild_palette_file_candidates();
                cx.notify();
            });
        });
        self._load_diff_task = Some(task);
    }

    /// Appends `path` to [`Self::open_files`] if not already present. The shared entry point for
    /// every source that opens a file tab, so the tab list can't drift from what `open_change`
    /// points at.
    fn push_open_file(&mut self, path: &Path) {
        if !self.open_files.iter().any(|open| open.as_path() == path) {
            self.open_files.push(path.to_path_buf());
        }
    }

    /// Opens `path`'s diff in the centre pane (the Changes row click handler).
    pub(crate) fn open_change_diff(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_code_surface(window, cx);
        self.push_open_file(&path);
        self.open_change = Some(path.clone());
        self.code_view = code_view::CodeView::Diff;
        self.refresh_open_diff_file_cache();
        // A hover card is only valid for the file it was requested against - and so is a real
        // Completions popup (Revision R8.5b audit finding 3's fix for a real, live-reproduced
        // data-corruption bug: without this, a popup left open from switching away from a file
        // could resurrect and splice stale text into whatever's active when the same path
        // becomes active again - see `Self::dismiss_completions`'s own docs).
        self.hover = None;
        self.dismiss_completions();
        self.close_tab_confirm_armed = None;
        cx.notify();
    }

    /// Opens `path` directly in Surface C's File view (the Files-tree row click handler).
    pub(crate) fn open_file_view(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.focus_code_surface(window, cx);
        // Clear any stale pending cursor line from an abandoned navigation;
        // `navigate_to_definition` re-sets it right after this if it has a target line.
        self.pending_cursor_line = None;
        let relative = path
            .strip_prefix(&self.file_tree_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        self.push_open_file(&relative);
        self.open_change = Some(relative.clone());
        self.code_view = code_view::CodeView::File;
        // Every "this file is now the selected tree row" path reveals it (GitHub issue #18 §5).
        // A click in the tree has its ancestors expanded already, so this is a no-op there - but
        // go-to-definition (`Self::navigate_to_definition`) lands on files in folders nobody has
        // expanded, and now that the tree starts collapsed, highlighting a row that isn't
        // showing would be no highlight at all.
        self.reveal_in_tree(&path, cx);
        self.selected_tree_path = Some(path);
        self.refresh_open_diff_file_cache();
        // See `Self::select_worktree`'s identical reset for why - and `Self::open_change_diff`'s
        // sibling `dismiss_completions()` call for the real data-corruption bug closing this
        // alongside `hover` prevents (Revision R8.5b audit finding 3).
        self.hover = None;
        self.dismiss_completions();
        self.close_tab_confirm_armed = None;
        cx.notify();
    }

    /// Activates a file tab (the tab strip's click handler), as opposed to
    /// [`Self::open_change_diff`]/[`Self::open_file_view`], which open a file that may not have a
    /// tab yet. Calls [`Self::push_open_file`] itself so `open_change` can never point at a path
    /// missing from `open_files`.
    ///
    /// Switching to a different tab with a diff resets [`Self::code_view`] to `Diff`, matching a
    /// freshly opened changed file, rather than inheriting whatever `Diff`/`File` toggle state a
    /// different file left `code_view` in (it's a single global field, not per-tab).
    ///
    /// Re-clicking the already-"active" tab is not always a no-op: the tab can be active without
    /// being shown (e.g. its diff disappeared after a revert, so [`Self::render_center_pane`]
    /// falls back to the active session while the tab strip still marks it active). That case
    /// falls back to `code_view = File`, which always has content to show.
    pub(crate) fn activate_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_open_file(&path);

        if self.open_change.as_deref() == Some(path.as_path()) {
            self.code_view = code_view::CodeView::File;
            cx.notify();
            return;
        }

        self.focus_code_surface(window, cx);
        let has_diff = self
            .current_diff()
            .is_some_and(|diff| diff.files.iter().any(|file| file.path == path));
        self.open_change = Some(path.clone());
        self.code_view = if has_diff {
            code_view::CodeView::Diff
        } else {
            code_view::CodeView::File
        };
        self.refresh_open_diff_file_cache();
        self.hover = None;
        // See `Self::open_change_diff`'s identical `dismiss_completions()` call for why
        // (Revision R8.5b audit finding 3).
        self.dismiss_completions();
        self.code_cursor = None;
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.close_tab_confirm_armed = None;
        cx.notify();
    }

    /// Closes file tab `path`, removing it from [`Self::open_files`] (the tab's `×` hit box). If
    /// `path` was the active tab, activates the neighbor to its right, else the one to its left,
    /// else falls back to the active session's terminal (restoring focus like
    /// [`Self::close_settings`] does). No-op if `path` isn't an open tab.
    ///
    /// Cancels any real, in-flight debounced LSP sync/completion-request task for `path` (via
    /// [`Self::_lsp_sync_tasks`]) and drops a real, stale [`Self::completions`] popup for it, if
    /// one is open - Revision R8.5b audit finding 3's fix for a real, live-reproduced data-
    /// corruption bug: without this, a completions popup requested against `path` could survive
    /// its own tab closing entirely, then resurrect and let stale, wrong-context text be spliced
    /// into whatever file is active if `path`'s tab (with the same buffer, still held in
    /// [`Self::edit_buffers`] - this viewer never actually drops a buffer on tab close, only its
    /// tab entry) is reopened later. The buffer itself is deliberately *not* dropped here (a real
    /// tab close is not a "discard this file's edits" action - reopening the same path restores
    /// its real, still-unsaved content), only the completions/sync state tied to the tab that no
    /// longer exists.
    /// Real entry point for every real close gesture (GitHub issue #26): the tab strip's `×`,
    /// middle-click, and the global `Ctrl+W`/[`crate::root::CloseFocusedTab`] action all call this
    /// instead of [`Self::close_file_tab`] directly, so none of them can bypass the real unsaved-
    /// changes confirmation below.
    ///
    /// A tab whose [`crate::code_surface::edit_buffer::EditBuffer::is_dirty`] is `false` closes
    /// immediately - there is nothing real to lose (and, per [`Self::close_file_tab`]'s own docs,
    /// this app doesn't even drop the buffer on an ordinary close - reopening the same path
    /// restores it - so a prompt there would be friction over nothing). A *dirty* tab needs one
    /// real confirming gesture on the same `path` first: the first call arms
    /// [`AdeApp::close_tab_confirm_armed`] (which `crate::work_surface::render`'s tab renderers
    /// read to show a real, visible "close without saving?" cue - never a silent internal flag
    /// with no on-screen feedback) and returns without closing anything; a second call while still
    /// armed for the *same* `path` disarms and really closes it - the same real two-gesture
    /// idiom [`Self::request_prune`]/[`crate::worktree_history::flow::AdeApp::
    /// request_discard_worktree`] already establish for this app's other destructive-feeling
    /// actions, reused here rather than a third, independently-invented confirmation mechanism.
    pub(crate) fn request_close_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_dirty = self
            .edit_buffers
            .get(&path)
            .is_some_and(|buffer| buffer.is_dirty());
        if is_dirty && self.close_tab_confirm_armed.as_deref() != Some(path.as_path()) {
            self.close_tab_confirm_armed = Some(path);
            cx.notify();
            return;
        }
        self.close_tab_confirm_armed = None;
        self.close_file_tab(path, window, cx);
    }

    pub(crate) fn close_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.open_files.iter().position(|open| open == &path) else {
            return;
        };
        if self.close_tab_confirm_armed.as_deref() == Some(path.as_path()) {
            self.close_tab_confirm_armed = None;
        }
        self.open_files.remove(index);
        self._lsp_sync_tasks.remove(&path);
        if self
            .completions
            .as_ref()
            .is_some_and(|entry| entry.path == path)
        {
            self.dismiss_completions();
        }
        let was_active = self.open_change.as_deref() == Some(path.as_path());
        if was_active {
            // After removal, the tab that was to the right (if any) has shifted into `index`;
            // fall back to `index - 1` only if there's nothing there.
            let neighbor = self.open_files.get(index).cloned().or_else(|| {
                index
                    .checked_sub(1)
                    .and_then(|left| self.open_files.get(left).cloned())
            });
            match neighbor {
                Some(next_path) => {
                    self.open_change = Some(next_path.clone());
                    self.refresh_open_diff_file_cache();
                    self.hover = None;
                    self.dismiss_completions();
                }
                None => {
                    self.open_change = None;
                    self.refresh_open_diff_file_cache();
                    self.hover = None;
                    self.dismiss_completions();
                    restore_focus(&self.sessions, &mut self.code_focus, window, cx);
                }
            }
        }
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// Closes the currently active file tab, if any (the code surface toolbar's own "× close",
    /// distinct from the tab strip's). Calls [`Self::close_file_tab`] directly - **not**
    /// [`Self::request_close_file_tab`]'s dirty-tab confirm-arm gate. GitHub issue #26 briefly
    /// routed this button through that gate too, but never gave it a matching visible "close
    /// without saving?" cue the way `crate::work_surface::render`'s tab-strip `×` has
    /// (`Self::close_tab_confirm_armed`'s own docs) - that issue's own `BUILD-LOG.md` entry lists
    /// "the global `Ctrl+W` binding, the tab strip's own `×`, and middle-click" as the real
    /// affordances sharing the gate, and doesn't mention this one, and its own verification was
    /// scoped to `code_surface::`/`settings::`/`keymap*` tests, which never included
    /// `root::focus::text_undo_scoping_tests` - so a dirty file's first click here silently armed
    /// an invisible confirm state with zero on-screen feedback, unnoticed. Per
    /// [`Self::close_file_tab`]'s own docs, nothing is actually destroyed by closing either way -
    /// the edit buffer (and its whole undo history) stays alive in [`Self::edit_buffers`], and
    /// reopening the same path restores it - so skipping the confirm here trades a real but minor
    /// inconsistency (this button behaving differently from the tab strip's own `×`) for avoiding
    /// a real, silent, first-click-does-nothing regression, which this project's own bug-class
    /// history (this file's module docs) already treats as the worse failure mode.
    pub(crate) fn close_change_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.open_change.clone() {
            self.close_file_tab(path, window, cx);
        }
    }

    /// Recomputes [`Self::open_diff_file_cache`] (and [`Self::file_view_changed_lines`] with it)
    /// from [`Self::open_change`] and [`Self::current_diff`]. Called whenever either input
    /// changes; never from a render method, to avoid a per-render `DiffFile` clone - also the
    /// real hook [`Self::ensure_diff_highlight_cache`] recomputes real syntax highlighting from,
    /// for the same reason (see that method's own docs).
    pub(crate) fn refresh_open_diff_file_cache(&mut self) {
        self.open_diff_file_cache = match (&self.open_change, self.current_diff()) {
            (Some(open_path), Some(diff)) => diff
                .files
                .iter()
                .find(|file| &file.path == open_path)
                .cloned(),
            _ => None,
        };
        self.file_view_changed_lines = self
            .open_diff_file_cache
            .as_ref()
            .map(code_view::changed_line_set)
            .unwrap_or_default();
        self.ensure_diff_highlight_cache();
    }

    /// Dispatches a background `code_view::load_file` call for `path` (must never run inline
    /// during `render()`; see [`FileLoadState`]'s docs). Mirrors [`Self::load_diff`]'s shape: mark
    /// `Loading` and notify immediately, run the blocking work on `cx.background_executor()`, then
    /// write the outcome back into [`Self::file_view_cache`]/[`Self::file_load_state`].
    pub(in crate::code_surface) fn spawn_file_load(
        &mut self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        self.file_load_state = FileLoadState::Loading(path.clone());
        cx.notify();
        // The worktree-relative key `Self::edit_buffers` uses, matching `Self::open_files`' own
        // convention - computed synchronously here (a cheap prefix-strip, no I/O) so the
        // background closure below can move it in without needing `&self` later.
        let relative_path = path
            .strip_prefix(&self.file_tree_root)
            .map(|stripped| stripped.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_string());
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { code_view::load_file_with_source(&path) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((parsed, source)) => {
                        // Lazily seed a real edit buffer the first time this file is opened in
                        // File view - never overwrite an existing entry (which may hold real
                        // unsaved edits and a live cursor/selection); see `Self::edit_buffers`'
                        // own docs. A truncated file (see `code_view::MAX_FILE_BYTES`) is
                        // deliberately excluded: editing a partial view and saving it would
                        // silently discard everything past the cap - the same reasoning that
                        // already justifies the cap itself - so such a file stays read-only. A
                        // file whose real bytes aren't valid UTF-8 is excluded for the same real
                        // reason: `code_view::load_file_with_source`'s `source` is already a
                        // lossy `String::from_utf8_lossy` decode at that point, with every
                        // invalid byte sequence silently replaced by `U+FFFD` - seeding an
                        // editable buffer from it and later saving would overwrite the file's
                        // real original bytes with those replacement characters. `parsed.
                        // is_valid_utf8` is the exact real flag `code_view::load_file` already
                        // computes for this (already surfaced in the status bar's `UTF-8`
                        // label) - reused here rather than a second check.
                        // `Some(line)` only when an *existing*, clean buffer was reloaded in
                        // place below - see that arm's own docs.
                        let mut reloaded_cursor_line: Option<usize> = None;
                        // Set alongside it, so the language-server sync can run after the
                        // `edit_buffers` borrow above has ended.
                        let mut reloaded = false;
                        // A write this app itself issued for this exact path is still queued or
                        // running, so anything this background read saw is by definition not the
                        // final on-disk state - and the `(mtime, len)` guard below can't catch it
                        // if two of our own writes land inside one filesystem mtime granularity
                        // tick. Never adopt a read that raced our own writer; the next freshness
                        // tick re-reads once the writer has drained. Read before the
                        // `edit_buffers` borrow starts.
                        let save_in_flight = this.file_save_pending.contains(&relative_path)
                            || this.file_save_running.contains(&relative_path);
                        if !parsed.truncated && parsed.is_valid_utf8 {
                            match this.edit_buffers.get_mut(&relative_path) {
                                // An existing buffer with **no** unsaved edits, whose file has
                                // since been rewritten on disk by someone else (an agent CLI
                                // running in this very worktree - this app's whole domain - a
                                // formatter, another editor). Adopting that content is the honest
                                // result: this buffer has nothing of the user's to lose, and
                                // leaving it showing bytes that no longer exist anywhere would be
                                // silently stale. Recorded as one single undoable step rather
                                // than a fresh buffer, per GitHub issue #17: an external rewrite
                                // must never be a silent history wipe mid-stack, and Ctrl+Z
                                // straight after one really does put the pre-reload content back.
                                // See `EditBuffer::reload_from_disk`'s own docs.
                                //
                                // The `(mtime, len)` guard is a real staleness check, not
                                // belt-and-braces: this read started before the `this.update`
                                // it is now inside, and a real, documented user action can land in
                                // between - `EditorSaveAnyway` (`force_save_active_file`), the
                                // explicit escape hatch for an external-change conflict, writes the
                                // user's own content and marks the buffer clean again. Without this
                                // check, the now-stale read would then be adopted over content the
                                // user had *just* deliberately force-saved, and would stamp the
                                // buffer's `saved_mtime`/`saved_len` to an on-disk identity the
                                // file no longer has. Refusing anything not strictly newer than
                                // what the buffer already believes about disk closes that window;
                                // the next freshness tick re-reads and gets it right.
                                Some(buffer)
                                    if !buffer.is_dirty()
                                        && (parsed.mtime, parsed.len)
                                            != (buffer.saved_mtime, buffer.saved_len)
                                        && parsed.mtime >= buffer.saved_mtime
                                        && !save_in_flight =>
                                {
                                    buffer.reload_from_disk(
                                        source,
                                        parsed.lines.clone(),
                                        parsed.mtime,
                                        parsed.len,
                                    );
                                    // The reloaded content is genuinely different text at genuinely
                                    // different offsets - every other content mutation in this
                                    // crate pairs with an LSP sync (see `Self::step_edit_history`),
                                    // and skipping it here would leave the language server
                                    // answering hover/diagnostics/goto about a document that is no
                                    // longer on screen until the user's next keystroke.
                                    reloaded = true;
                                    // `reload_from_disk` keeps the real caret (clamped into the
                                    // new content), so the caret-line indicator below must follow
                                    // it rather than snapping back to line 1 the way a genuinely
                                    // fresh load's does - the buffer's own caret is the truth
                                    // here, and an indicator disagreeing with it would be a real,
                                    // visible lie.
                                    let (line, _) =
                                        buffer.line_col_for_offset(buffer.cursor_offset());
                                    reloaded_cursor_line = Some(line + 1);
                                }
                                // A **dirty** buffer is deliberately left completely alone: its
                                // unsaved content is the user's, and this app already surfaces the
                                // real divergence as an explicit conflict
                                // (`AdeApp::file_external_conflict`, plus the save-time refusal in
                                // `save_active_file`) for the user to resolve, rather than picking
                                // a winner for them. Nothing touches the history in this case
                                // either - no wipe, no boundary, because no edit happened.
                                Some(_) => {}
                                // First open of this file in the editable File view - see this
                                // block's own docs above for why a truncated/invalid-UTF-8 file
                                // deliberately never reaches here at all.
                                None => {
                                    this.edit_buffers.insert(
                                        relative_path.clone(),
                                        edit_buffer::EditBuffer::from_highlighted(
                                            path.clone(),
                                            source,
                                            extension.clone(),
                                            parsed.lines.clone(),
                                            parsed.mtime,
                                            parsed.len,
                                        ),
                                    );
                                }
                            }
                        }
                        if reloaded {
                            this.schedule_lsp_sync(relative_path.clone(), cx);
                        }
                        this.file_view_cache = Some(parsed);
                        // A pending go-to-definition target line applies only if it names the
                        // file that just finished loading, so an unrelated file's load can't
                        // apply a stale target left over from an abandoned navigation.
                        let target_line = match &this.pending_cursor_line {
                            Some((pending_path, line)) if pending_path == &path => Some(*line),
                            _ => None,
                        };
                        if let Some(line) = target_line {
                            this.pending_cursor_line = None;
                            this.file_view_scroll_handle
                                .scroll_to_item(line.saturating_sub(1), ScrollStrategy::Center);
                        }
                        this.code_cursor = Some(target_line.or(reloaded_cursor_line).unwrap_or(1));
                        this.file_load_state = FileLoadState::Idle;
                    }
                    Err(error) => {
                        this.file_load_state =
                            FileLoadState::Error(path.clone(), error.to_string());
                        // Don't leave a stale target line to misapply onto the next file that
                        // loads successfully.
                        this.pending_cursor_line = None;
                    }
                }
                cx.notify();
            });
        });
        self._file_load_task = Some(task);
    }

    /// Handles `TerminalPaneEvent::OpenPath` - a mod-held click on a detected path/`path:line`
    /// link in a session's terminal output. `path` is already resolved against the session's cwd
    /// (see `crate::terminal::links::resolve`). Reuses [`Self::navigate_to_definition`] when the
    /// link carried a line number, else [`Self::open_file_view`].
    ///
    /// Unlike every other caller of `open_file_view`, a terminal link's path isn't guaranteed to
    /// exist: `crate::terminal::links`'s regex is a heuristic over plain text, not a filesystem lookup.
    /// The synchronous `Path::is_file()` check is affordable here since it runs once per click,
    /// not per render; without it, a false-positive link would open a permanent junk tab.
    pub(crate) fn open_terminal_link(
        &mut self,
        path: PathBuf,
        line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !path.is_file() {
            log::debug!(
                "terminal link click: {} does not exist on disk, ignoring",
                path.display()
            );
            return;
        }
        match line {
            Some(line) => self.navigate_to_definition(path, line as usize, window, cx),
            None => self.open_file_view(path, window, cx),
        }
    }

    /// The currently loaded diff, if any - `None` while loading/erroring, or when the worktree is
    /// on its default branch / has no detectable base (see [`wt_core::diff::DiffBase`]). The
    /// single source every view that shows diff state reads.
    pub(crate) fn current_diff(&self) -> Option<&WorktreeDiff> {
        match &self.diff_state {
            DiffLoadState::Loaded(DiffBase::Diff(diff)) => Some(diff),
            _ => None,
        }
    }
}

/// Regression coverage for `AdeApp::render_file_view`'s cache: re-running an expensive parse on
/// every render.
#[cfg(test)]
mod code_view_cache_tests {
    use super::*;
    use gpui::TestAppContext;

    /// A direct wall-clock proof that opening a large file no longer blocks `render_center_pane`
    /// on the full `load_file` parse: a timing comparison against a synchronous baseline on the
    /// same file, same machine, same test run (a ratio, not an absolute threshold, so it isn't
    /// flaky under CI load).
    ///
    /// Uses this crate's own `lsp/client.rs` - its largest single source file - as the large
    /// `.rs` fixture. (It was `root/code_surface.rs` before that file was split into this folder.)
    #[gpui::test]
    fn opening_a_large_real_file_does_not_block_render_on_the_full_parse(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("large.rs");
        let source =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/lsp/client.rs"))
                .expect("read this crate's own lsp/client.rs as a real, large .rs fixture");
        std::fs::write(&file_path, &source).expect("write large.rs");

        // The synchronous baseline: how long the blocking read+parse takes on this machine.
        let baseline_start = std::time::Instant::now();
        code_view::load_file(&file_path).expect("load_file baseline");
        let baseline_duration = baseline_start.elapsed();

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });

        let render_start = std::time::Instant::now();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let render_duration = render_start.elapsed();

        assert!(
            app.read_with(cx, |app, _| app.file_view_cache.is_none()),
            "the real parse must not have completed synchronously inside this render call"
        );
        assert!(
            render_duration < baseline_duration,
            "render_center_pane's own foreground-thread duration ({render_duration:?}) should \
             be far less than a real, direct, synchronous code_view::load_file call on the same \
             file ({baseline_duration:?}) - render only dispatches the load to the background \
             executor and returns immediately, it does not run the parse inline"
        );

        // Drive the background load to completion and confirm the whole file loaded.
        cx.run_until_parked();
        let cached_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated once the background load completed")
                .lines
                .len()
        });
        assert!(
            cached_line_count > 1000,
            "sanity check: this should have really been a large, multi-thousand-line file, not \
             an accidentally-empty fixture"
        );
    }

    /// Confirms two things: (1) the parse happens off the foreground thread - `file_view_cache`
    /// is still `None` right after the render that kicks off the load, before
    /// `run_until_parked()` drives it to completion; (2) once loaded, further re-renders reuse
    /// the cached parse - proven by pointer identity of `ParsedFile::lines`, since a fresh
    /// `load_file` call would allocate a new `Vec`.
    #[gpui::test]
    fn repeated_renders_of_the_same_open_file_reuse_the_cached_parse(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(
            &file_path,
            "fn add(left: i32, right: i32) -> i32 {\n    left + right\n}\n",
        )
        .expect("write sample.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.file_view_cache.is_none()),
            "file_view_cache must still be empty immediately after the render that dispatched \
             the load - the real parse has not run yet, since it only runs on the background \
             executor; if this is already populated here, the parse ran synchronously inline \
             during render() again"
        );

        // Drives `spawn_file_load`'s background task, and its write-back, to completion.
        cx.run_until_parked();

        let first_render_ptr = app.update(cx, |app, cx| {
            app.render_center_pane(cx);
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated once the background load completed")
                .lines
                .as_ptr()
        });

        for render_index in 1..=3 {
            let ptr = app.update(cx, |app, cx| {
                app.render_center_pane(cx);
                app.file_view_cache
                    .as_ref()
                    .expect("file_view_cache should stay populated across re-renders")
                    .lines
                    .as_ptr()
            });
            assert_eq!(
                ptr, first_render_ptr,
                "render #{render_index} of the same, unchanged open file rebuilt the cached \
                 parse instead of reusing it (a fresh heap allocation for `ParsedFile::lines` \
                 means `code_view::load_file` ran again)"
            );
        }
    }

    /// A content change (different mtime/len) must invalidate the cache - confirms this isn't a
    /// cache that never refreshes. Sleeps past [`FILE_FRESHNESS_CHECK_INTERVAL`] first, since the
    /// throttle window itself is covered separately by
    /// `renders_within_the_throttle_window_do_not_pick_up_a_fresh_on_disk_change` below.
    #[gpui::test]
    fn a_real_on_disk_change_to_the_open_file_invalidates_the_cache(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add() -> i32 {\n    1\n}\n").expect("write sample.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let original_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the first real load")
                .lines
                .len()
        });

        // A content change with more lines than before.
        std::fs::write(
            &file_path,
            "fn add() -> i32 {\n    1\n}\n\nfn subtract() -> i32 {\n    -1\n}\n",
        )
        .expect("rewrite sample.rs");

        // Past the throttle window, so the next freshness check isn't skipped.
        std::thread::sleep(FILE_FRESHNESS_CHECK_INTERVAL + std::time::Duration::from_millis(50));

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let updated_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the second real load")
                .lines
                .len()
        });

        assert!(
            updated_line_count > original_line_count,
            "a real on-disk change to the open file should have invalidated the cache and \
             reloaded its real, larger content"
        );
    }

    /// Proves [`AdeApp::file_view_last_freshness_check`]'s throttling: a change made within
    /// [`FILE_FRESHNESS_CHECK_INTERVAL`] of the last check isn't picked up yet, but the same
    /// change is picked up once the window passes.
    #[gpui::test]
    fn renders_within_the_throttle_window_do_not_pick_up_a_fresh_on_disk_change(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn add() -> i32 {\n    1\n}\n").expect("write sample.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let original_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the first real load")
                .lines
                .len()
        });

        // A content change made immediately, within the throttle window.
        std::fs::write(
            &file_path,
            "fn add() -> i32 {\n    1\n}\n\nfn subtract() -> i32 {\n    -1\n}\n",
        )
        .expect("rewrite sample.rs");

        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        let still_original_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("cache must still be populated - no reload should have been dispatched")
                .lines
                .len()
        });
        assert_eq!(
            still_original_line_count, original_line_count,
            "a render inside the throttle window re-`stat`'d the file and picked up the on-disk \
             change early - the freshness check should have been skipped this render"
        );
        assert!(
            app.read_with(cx, |app, _| matches!(
                app.file_load_state,
                FileLoadState::Idle
            )),
            "no reload should have been dispatched while the freshness check was throttled"
        );

        // Past the throttle window - the change is now observed.
        std::thread::sleep(FILE_FRESHNESS_CHECK_INTERVAL + std::time::Duration::from_millis(50));
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        let updated_line_count = app.read_with(cx, |app, _| {
            app.file_view_cache
                .as_ref()
                .expect("file_view_cache should be populated after the throttled-then-real load")
                .lines
                .len()
        });
        assert!(
            updated_line_count > original_line_count,
            "once the throttle window passed, the same real on-disk change should finally have \
             been picked up"
        );
    }
}

/// Regression coverage for the cross-file cursor leak [`AdeApp::pending_cursor_line`] describes:
/// [`AdeApp::navigate_to_definition`] to a file B that isn't cached yet leaves a one-shot target
/// line waiting for B's background load; opening a different file C before that resolves must
/// not let C's load misapply B's stale target line.
#[cfg(test)]
mod cross_file_navigation_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn navigating_to_an_uncached_file_then_opening_a_different_file_first_does_not_leak_the_cursor(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_b = repo.path().join("b.txt");
        let file_c = repo.path().join("c.txt");
        std::fs::write(&file_b, "one\ntwo\nthree\nfour\nfive\n").expect("write b.txt");
        std::fs::write(&file_c, "hello\nworld\n").expect("write c.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        // Landing on B's line 5 - B isn't cached yet, so this sets `pending_cursor_line` rather
        // than `code_cursor` directly.
        app.update_in(cx, |app, window, cx| {
            app.navigate_to_definition(file_b.clone(), 5, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.pending_cursor_line.clone()),
            Some((file_b.clone(), 5))
        );
        app.update(cx, |app, cx| {
            // Dispatches B's background load; not yet driven to completion, so it's still
            // in flight.
            app.render_center_pane(cx);
        });

        // Before B's load resolves, the user opens unrelated file C.
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_c.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            // Dispatches C's background load, dropping (and so cancelling) B's in-flight one.
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let (loaded_path, cursor) = app.read_with(cx, |app, _| {
            (
                app.file_view_cache
                    .as_ref()
                    .map(|parsed| parsed.path.clone()),
                app.code_cursor,
            )
        });
        assert_eq!(
            loaded_path,
            Some(file_c.clone()),
            "sanity check: C's own real load should have completed"
        );
        assert_eq!(
            cursor,
            Some(1),
            "C is a fresh file open with no navigation target of its own - its cursor must start \
             at real line 1, not leak B's stale target line 5"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.pending_cursor_line.clone()),
            None,
            "AdeApp::open_file_view must have cleared the stale entry outright when C was opened, \
             so it can never leak onto a later, unrelated file's load either"
        );
    }
}

/// Regression coverage for the unbounded busy-loop [`FileLoadState::Error`] describes: a read
/// failure must settle into a stable error state, never respawn a doomed load every render.
#[cfg(test)]
mod unreadable_file_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn an_unreadable_file_settles_into_a_stable_error_state_without_respawning(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        // A deterministic, cross-platform read failure: no such path exists, so `load_file`'s
        // `fs::metadata`/`fs::read` fail every time, with no platform-specific setup needed.
        let missing_path = repo.path().join("does-not-exist.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(missing_path.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let message = app.read_with(cx, |app, _| match &app.file_load_state {
            FileLoadState::Error(path, message) => {
                assert_eq!(path, &missing_path);
                message.clone()
            }
            other => panic!("expected a real Error state after the failed load, got {other:?}"),
        });

        // The regression this guards against: before the fix, this next render alone (no
        // further `run_until_parked()`) would flip `file_load_state` back to `Loading`, because
        // the respawn guard only checked `FileLoadState::Loading`, never `Error` - an unbounded
        // busy-loop instead of a stable error state.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        app.read_with(cx, |app, _| match &app.file_load_state {
            FileLoadState::Error(path, message_after) => {
                assert_eq!(path, &missing_path);
                assert_eq!(
                    message_after, &message,
                    "the same real, already-observed error - not a freshly respawned attempt"
                );
            }
            other => panic!(
                "a real, already-observed read failure for this exact path must stay a stable \
                 Error state across further renders, not respawn back into Loading - got \
                 {other:?}"
            ),
        });
    }
}

/// Regression coverage for the multi-file tab strip's `open_files` state transitions:
/// [`AdeApp::open_change_diff`]/[`AdeApp::open_file_view`] no longer discard a file when the user
/// navigates elsewhere, [`AdeApp::activate_file_tab`] switches which one is showing without
/// touching the list, and [`AdeApp::close_file_tab`] is the only place a tab leaves the list.
#[cfg(test)]
mod multi_file_tab_tests {
    use super::*;
    use gpui::TestAppContext;

    /// Writes three files under `dir` and returns both their absolute paths (what
    /// `open_file_view` takes) and their repo-relative paths (what `open_change`/`open_files`/
    /// `activate_file_tab`/`close_file_tab` key by, via `open_file_view`'s `strip_prefix`).
    fn write_three_files(
        dir: &std::path::Path,
    ) -> ((PathBuf, PathBuf, PathBuf), (PathBuf, PathBuf, PathBuf)) {
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        let c = dir.join("c.txt");
        std::fs::write(&a, "a\n").expect("write a.txt");
        std::fs::write(&b, "b\n").expect("write b.txt");
        std::fs::write(&c, "c\n").expect("write c.txt");
        let rel = (
            PathBuf::from("a.txt"),
            PathBuf::from("b.txt"),
            PathBuf::from("c.txt"),
        );
        ((a, b, c), rel)
    }

    /// Opening the same file twice must not append a second tab - `Self::push_open_file`'s own
    /// real no-duplicate rule.
    #[gpui::test]
    fn opening_the_same_file_twice_does_not_duplicate_its_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let ((a, _b, _c), (a_rel, _, _)) = write_three_files(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files.len()),
            1,
            "opening an already-open file a second time must activate the existing tab, not \
             append a duplicate"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel)
        );
    }

    /// Closing the active tab activates a sensible neighbor: the tab that was to its right first
    /// - `Self::close_file_tab`'s own documented "prefer right, then left, then fall back to the
    /// active session" order.
    #[gpui::test]
    fn closing_the_active_tab_activates_the_tab_that_was_to_its_right(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let ((a, b, c), (a_rel, b_rel, c_rel)) = write_three_files(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        for path in [a, b, c] {
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(path, window, cx);
            });
            cx.run_until_parked();
        }
        // Opening a, then b, then c in order leaves `open_files == [a, b, c]` with `c` (the
        // most recently opened) active - reactivate the middle one (`b`) so there's a real
        // right *and* left neighbor to prove the "prefer right" order against.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(b_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(b_rel.clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(b_rel, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files.clone()),
            vec![a_rel.clone(), c_rel.clone()],
            "b should be gone from the tab list"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(c_rel),
            "closing the active middle tab should activate the tab that was to its right, not \
             the one to its left"
        );

        // Closing the now-active last tab (`c`) has no tab to its right left, so it should fall
        // back to the one on its left (`a`).
        app.update_in(cx, |app, window, cx| {
            let active = app.open_change.clone().expect("c should be active");
            app.close_file_tab(active, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel)
        );

        // Closing the last remaining tab falls all the way back to no active file at all (the
        // active session shows through instead).
        app.update_in(cx, |app, window, cx| {
            let active = app.open_change.clone().expect("a should be active");
            app.close_file_tab(active, window, cx);
        });
        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);
        assert!(app.read_with(cx, |app, _| app.open_files.is_empty()));
    }

    /// Closing a tab that is *not* the active one must not change what's currently active - the
    /// other real half of [`AdeApp::close_file_tab`]'s contract.
    #[gpui::test]
    fn closing_a_non_active_tab_does_not_change_what_is_active(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let ((a, b, _c), (a_rel, b_rel, _)) = write_three_files(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b, window, cx);
        });
        cx.run_until_parked();
        // Reactivate `a` so `b` (opened more recently) is the real non-active tab under test.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel.clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(b_rel, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel),
            "closing a tab that wasn't active must leave the active tab unchanged"
        );
        assert_eq!(app.read_with(cx, |app, _| app.open_files.len()), 1);
    }

    /// Clicking a session tab while a file tab is active deactivates the file (it stops being
    /// shown) without closing it - it stays in `open_files`, exactly like switching away from a
    /// browser tab doesn't close it.
    #[gpui::test]
    fn selecting_a_session_deactivates_the_open_file_tab_without_closing_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let ((a, _b, _c), (a_rel, _, _)) = write_three_files(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let session_id = app.read_with(cx, |app, _| {
            app.sessions
                .active_id()
                .expect("a fresh window has one real session")
        });

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(a_rel.clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.select_session(session_id, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            None,
            "selecting a session tab should deactivate the file tab"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files.clone()),
            vec![a_rel],
            "the file tab must still be in open_files, just not active"
        );

        // The centre pane must actually render the session again, not silently panic/show
        // nothing - a real smoke check on top of the state assertions above.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
    }

    /// [`AdeApp::next_changed_file`]'s "no active file -> first entry, otherwise advance,
    /// wrapping past the last entry" behavior, against a real git-backed diff so this also
    /// exercises `Self::current_diff`'s data.
    #[gpui::test]
    fn next_changed_file_advances_through_every_changed_file_and_wraps_around(
        cx: &mut TestAppContext,
    ) {
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

        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\n").expect("write b.txt");
        std::fs::write(repo.path().join("c.txt"), "1\n").expect("write c.txt");
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-m", "initial"]);
        git(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");
        std::fs::write(repo.path().join("b.txt"), "1\nchanged\n").expect("rewrite b.txt");
        std::fs::write(repo.path().join("c.txt"), "1\nchanged\n").expect("rewrite c.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let order: Vec<PathBuf> = app.read_with(cx, |app, _| {
            app.current_diff()
                .expect("a real diff against main should have loaded")
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect()
        });
        assert_eq!(
            order.len(),
            3,
            "sanity check: all three files should be changed"
        );

        assert_eq!(app.read_with(cx, |app, _| app.open_change.clone()), None);

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[0].clone()),
            "with nothing active, the first real changed file should open"
        );

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[1].clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[2].clone())
        );

        app.update_in(cx, |app, window, cx| {
            app.next_changed_file(window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(order[0].clone()),
            "advancing past the last changed file should wrap around to the first"
        );
    }

    fn git_repo(dir: &std::path::Path, args: &[&str]) {
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

    /// Regression test for "switching tabs shows the wrong Diff/File view": `code_view` is a
    /// single global field, not per-tab, so switching from a tab left in `File` view to a
    /// different tab with a diff used to incorrectly stay in `File` view instead of forcing
    /// `Diff` back.
    #[gpui::test]
    fn switching_to_a_tab_with_a_real_diff_shows_the_diff_not_the_last_view_mode(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git_repo(repo.path(), &["init", "-b", "main"]);
        git_repo(repo.path(), &["config", "user.email", "test@example.com"]);
        git_repo(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("changed.txt"), "1\n").expect("write changed.txt");
        std::fs::write(repo.path().join("plain.txt"), "plain\n").expect("write plain.txt");
        git_repo(repo.path(), &["add", "."]);
        git_repo(repo.path(), &["commit", "-m", "initial"]);
        git_repo(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("changed.txt"), "1\nchanged\n")
            .expect("rewrite changed.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let changed_rel = PathBuf::from("changed.txt");
        let plain_abs = repo.path().join("plain.txt");

        // Open the diff file first - lands in Diff view, matching `open_change_diff`'s default.
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(changed_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff
        );

        // Open a second, unrelated, unchanged file - forced into File view (no diff).
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(plain_abs, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File
        );

        // Switch back to the changed file's tab - it has a diff, so it must show that again, not
        // inherit the File view the plain file left `code_view` in.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(changed_rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(changed_rel)
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "switching back to a tab with a real diff must show that diff, not the File view \
             the previously active tab happened to leave code_view in"
        );
    }

    /// Regression test for the "active but not actually showing" gap: a file tab can stay
    /// `open_change`-active even after its diff disappears (e.g. the underlying change was
    /// reverted), so `render_center_pane`'s `has_diff_or_file_view` check falls back to the
    /// active session while the tab strip still paints the file tab as active. Before the fix,
    /// `activate_file_tab` early-returned as a dead no-op whenever `path` already equalled
    /// `open_change`, so re-clicking such a tab did nothing.
    #[gpui::test]
    fn reactivating_a_tab_whose_diff_disappeared_shows_real_content_again(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        git_repo(repo.path(), &["init", "-b", "main"]);
        git_repo(repo.path(), &["config", "user.email", "test@example.com"]);
        git_repo(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        git_repo(repo.path(), &["add", "."]);
        git_repo(repo.path(), &["commit", "-m", "initial"]);
        git_repo(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let rel = PathBuf::from("a.txt");
        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(rel.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff
        );

        // Revert the change on disk and reload the diff - the file's `DiffFile` disappears from
        // the loaded diff, but `open_change` still names it (nothing closed the tab).
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("revert a.txt");
        app.update(cx, |app, cx| app.load_diff(repo.path().to_path_buf(), cx));
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app
                .current_diff()
                .is_some_and(|diff| diff.files.iter().any(|file| file.path == rel))),
            "sanity: the file should no longer be in the loaded diff"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_change.clone()),
            Some(rel.clone()),
            "the tab is still 'active' by name, even though the surface has nothing real left to \
             show for it"
        );

        // Re-click the same, now-content-less tab - before the fix this was a dead no-op.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(rel, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File,
            "re-activating a tab that's active-but-not-really-showing must fall back to File \
             view, which always has real content, instead of staying a dead no-op"
        );
    }
}

/// Revision R8.5b audit finding 3's direct regression coverage: a genuine data-corruption bug -
/// a stale [`AdeApp::completions`] popup surviving a tab switch/close and resurrecting later,
/// letting stale text be spliced into whatever file happens to become active again. The real
/// `Ready` popup state is seeded directly (no real LSP round trip needed to prove this real
/// navigation/bookkeeping bug - matching `multi_file_tab_tests`' own established precedent, and
/// `crate::lsp::client::lsp_diagnostics_wiring_tests` for the real, live end-to-end completions
/// proof this module doesn't duplicate).
#[cfg(test)]
mod stale_completions_popup_tests {
    use super::*;
    use crate::lsp::completion_popup::{CompletionsEntry, CompletionsStatus};
    use gpui::TestAppContext;

    fn write_two_files(dir: &std::path::Path) -> ((PathBuf, PathBuf), (PathBuf, PathBuf)) {
        let a = dir.join("a.rs");
        let b = dir.join("b.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&b, "fn b() {}\n").expect("write b.rs");
        ((a, b), (PathBuf::from("a.rs"), PathBuf::from("b.rs")))
    }

    fn fake_ready_entry(path: PathBuf, label: &str) -> CompletionsEntry {
        CompletionsEntry {
            path,
            status: CompletionsStatus::Ready {
                items: vec![lsp_core::lsp_types::CompletionItem {
                    label: label.to_string(),
                    ..Default::default()
                }],
                selected: 0,
            },
        }
    }

    /// The exact scenario the audit reproduced live: open a completions popup on file A, switch
    /// to file B, switch back to A - the popup must not resurrect. Exercised via
    /// [`AdeApp::activate_file_tab`] (the tab-strip click handler), the real code path a real tab
    /// switch drives.
    #[gpui::test]
    fn switching_tabs_away_and_back_does_not_resurrect_a_stale_completions_popup(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let ((a, b), (a_rel, b_rel)) = write_two_files(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        for path in [a, b] {
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(path, window, cx);
            });
            cx.run_until_parked();
        }
        // `open_file_view` activates whichever file was opened last (`b`) - reactivate `a` so a
        // real popup can be seeded "for" it.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.completions = Some(fake_ready_entry(a_rel.clone(), "stale_for_a"));
            cx.notify();
        });
        assert!(
            app.read_with(cx, |app, _| app.completions_open_for_active_path()),
            "sanity check: the seeded popup should genuinely be open for the active file, a"
        );

        // Switch to b - a real, ordinary tab switch, not a close.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(b_rel.clone(), window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "switching tabs away from a must drop the real completions popup that was open for \
             it, not merely hide it - a stale entry left behind is exactly the real, live-\
             reproduced bug this fix closes"
        );

        // Switch back to a - the real, load-bearing assertion: no resurrection.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel, window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "switching back to a must not resurrect the stale popup that was open for it before \
             the switch away - accepting it would splice stale, wrong-context text into the \
             buffer, the real data-corruption bug this fix closes"
        );
    }

    /// The second half of the audit's own scenario: closing a tab with an open popup, then
    /// opening a *different* file, must never show or let the user accept stale completions meant
    /// for the closed file - and, separately, reopening the *same*, closed path later must not
    /// resurrect it either (the buffer itself survives a tab close - see [`AdeApp::
    /// close_file_tab`]'s own docs - so the popup must be dropped independently of the buffer).
    #[gpui::test]
    fn closing_a_tab_with_an_open_popup_never_lets_it_resurface_for_another_file(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let ((a, b), (a_rel, _b_rel)) = write_two_files(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        for path in [a.clone(), b] {
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(path, window, cx);
            });
            cx.run_until_parked();
        }
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(a_rel.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.completions = Some(fake_ready_entry(a_rel.clone(), "stale_for_a"));
            cx.notify();
        });

        // Close a's tab while its popup is open.
        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(a_rel.clone(), window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "closing a tab with an open real completions popup must drop it, not leave it \
             dangling for a file that's no longer open"
        );
        assert!(
            app.read_with(cx, |app, _| !app.open_files.contains(&a_rel)),
            "sanity check: a's tab should genuinely be closed"
        );

        // Reopening the exact same path later (the buffer itself survives a close - see
        // `AdeApp::close_file_tab`'s own docs) must not resurrect the popup either.
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a, window, cx);
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.completions.is_none()),
            "reopening the same path after its tab was closed must not resurrect a stale popup \
             from before the close"
        );
    }
}

/// End-to-end proof of the "terminal is interactive" link-click-opens-a-file path:
/// `simulate_click` against the pane's own painted bounds drives `TerminalPane`'s `on_click`/
/// `cx.emit`, `Sessions::spawn`'s `cx.subscribe_in`, and `AdeApp::open_terminal_link` - the same
/// chain a real mouse click goes through.
#[cfg(test)]
mod terminal_link_click_tests {
    use super::*;
    use gpui::{
        point, Bounds, Entity, Modifiers, MouseButton, Pixels, Size, TestAppContext,
        VisualTestContext,
    };

    /// Injects a row of terminal text containing a `path:line` link on the third visible row
    /// (`"see src/main.rs:1 for it"`, 0-indexed row 2) into the active session's pane, and
    /// returns the painted geometry (`content_bounds`, cell size) needed to compute a click
    /// position.
    fn inject_link_row_and_measure(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
    ) -> (Bounds<Pixels>, Size<Pixels>) {
        let pane = app
            .read_with(cx, |app, _| app.sessions.active().map(|s| s.pane.clone()))
            .expect("a fresh test window has one real, active shell session");

        pane.update(cx, |pane, cx| {
            pane.inject_bytes_for_test(
                b"first line\r\nsecond line\r\nsee src/main.rs:1 for it",
                cx,
            );
        });
        // Lets the injected `cx.notify()` drive a real paint (populating `content_bounds`). The
        // session's `$SHELL` spawn may also make background progress here, but only ever appends
        // after the injected bytes, so it can't touch the link characters this test's
        // click-position math depends on.
        cx.run_until_parked();

        let (bounds, cell_size) = pane.update_in(cx, |pane, window, _cx| {
            (
                pane.content_bounds_for_test(),
                pane.cell_size_for_test(window),
            )
        });
        (
            bounds.expect("the pane must have painted at least once after run_until_parked"),
            cell_size,
        )
    }

    /// The pixel position of the middle of `"src/main.rs:1"` on row 2 of
    /// [`inject_link_row_and_measure`]'s injected text (`"see "` is a 4-character prefix) -
    /// geometry math off the pane's measured padding/line-height/cell-width constants, mirroring
    /// `vendor/zed/crates/editor/src/editor_tests.rs`'s `simulate_click` call sites
    /// (`text_origin + em_width * column`, `line_height * row`).
    fn link_click_position(bounds: Bounds<Pixels>, cell_size: Size<Pixels>) -> gpui::Point<Pixels> {
        let link_text = "src/main.rs:1";
        let prefix_chars = 4.0; // "see "
        let x = bounds.origin.x
            + px(crate::terminal::pane::PANE_PADDING_PX)
            + cell_size.width * (prefix_chars + link_text.chars().count() as f32 / 2.0);
        let y = bounds.origin.y
            + px(crate::terminal::pane::PANE_PADDING_PX)
            + cell_size.height * 2.0
            + cell_size.height / 2.0;
        point(x, y)
    }

    #[gpui::test]
    fn mod_click_on_a_detected_link_opens_the_real_file_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);

        cx.simulate_click(
            link_click_position(bounds, cell_size),
            Modifiers::secondary_key(),
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change,
                Some(PathBuf::from("src/main.rs")),
                "a real mod-held click on the detected link must open the real file as the \
                 active tab - got open_change = {:?}, open_files = {:?}",
                app.open_change,
                app.open_files,
            );
            assert!(
                app.open_files.contains(&PathBuf::from("src/main.rs")),
                "the opened file must appear in the real tab list too"
            );
        });
    }

    /// The mod-held-click gesture matters: a bare click on the same link must not navigate, so
    /// an ordinary click inside the terminal is never silently hijacked into a file navigation.
    #[gpui::test]
    fn a_bare_click_on_a_detected_link_does_not_open_anything(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);

        cx.simulate_click(link_click_position(bounds, cell_size), Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change, None,
                "a bare click (no mod held) on a detected link must not open anything"
            );
            assert!(
                app.open_files.is_empty(),
                "a bare click must not add a real tab either"
            );
        });
    }

    /// The bug `crate::terminal::pane::click_included_secondary_modifier` fixes:
    /// `gpui::ClickEvent::modifiers()` only reports the modifiers held at mouse-up
    /// (vendor/zed/crates/gpui/src/interactive.rs, `ClickEvent::modifiers`), so a click sequence
    /// that releases the modifier just before releasing the mouse button used to silently do
    /// nothing. Drives mouse-down/mouse-up separately (not `simulate_click`, which holds the
    /// same modifiers for both) to hold the modifier only at mouse-down.
    #[gpui::test]
    fn mod_held_only_during_mouse_down_still_opens_the_real_file_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(repo.path().join("src")).expect("mkdir src");
        std::fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);
        let position = link_click_position(bounds, cell_size);

        cx.simulate_mouse_down(position, MouseButton::Left, Modifiers::secondary_key());
        cx.simulate_mouse_up(position, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change,
                Some(PathBuf::from("src/main.rs")),
                "a modifier held only during mouse-down (released before mouse-up) must still \
                 open the real file tab, not be silently dropped - got open_change = {:?}",
                app.open_change,
            );
        });
    }

    /// A false-positive class `crate::terminal::links`'s regex can't rule out: a plausible-looking
    /// path that doesn't exist on disk. `open_terminal_link`'s `Path::is_file()` check must
    /// refuse it, not open a permanent junk tab.
    #[gpui::test]
    fn mod_click_on_a_link_to_a_nonexistent_path_does_not_open_a_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        // Deliberately never created: `src/` itself doesn't exist in this repo at all.

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let (bounds, cell_size) = inject_link_row_and_measure(&app, cx);

        cx.simulate_click(
            link_click_position(bounds, cell_size),
            Modifiers::secondary_key(),
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change, None,
                "a mod-click on a link that resolves to a real, nonexistent path must not open \
                 anything - got open_change = {:?}",
                app.open_change,
            );
            assert!(
                app.open_files.is_empty(),
                "a link to a nonexistent path must not add a real tab either"
            );
        });
    }
}
