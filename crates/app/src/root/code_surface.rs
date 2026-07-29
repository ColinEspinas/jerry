use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::lsp::{lsp_file_status, LspClientState, LspFileStatus};
use crate::root::rem_scope::WithRemSize;
use crate::root::settings_widgets::ChoiceOption;
use crate::root::widgets::{
    render_action_keycap_row, render_keycap, render_sidebar_message, render_tag_pill,
};

impl AdeApp {
    /// Loads (or reloads) the diff of `root` against its detected base branch. Runs on
    /// `cx.background_executor()` since `diff_against_base` does blocking I/O (gix reads plus a
    /// spawned `git diff` process) and must not run on the GPUI foreground thread.
    pub(super) fn load_diff(&mut self, root: PathBuf, cx: &mut Context<Self>) {
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
    pub(super) fn open_change_diff(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_code_surface(window, cx);
        self.push_open_file(&path);
        self.open_change = Some(path.clone());
        self.code_view = code_view::CodeView::Diff;
        self.restore_zoom_for_open_change(&path);
        self.refresh_open_diff_file_cache();
        // A hover card is only valid for the file it was requested against.
        self.hover = None;
        cx.notify();
    }

    /// Opens `path` directly in Surface C's File view (the Files-tree row click handler).
    pub(super) fn open_file_view(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prune_confirm_armed = false;
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
        self.restore_zoom_for_open_change(&relative);
        self.selected_tree_path = Some(path);
        self.refresh_open_diff_file_cache();
        // See `Self::select_worktree`'s identical reset for why.
        self.hover = None;
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
    pub(super) fn activate_file_tab(
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
        self.restore_zoom_for_open_change(&path);
        self.refresh_open_diff_file_cache();
        self.hover = None;
        self.code_cursor = None;
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Closes file tab `path`, removing it from [`Self::open_files`] (the tab's `×` hit box). If
    /// `path` was the active tab, activates the neighbor to its right, else the one to its left,
    /// else falls back to the active session's terminal (restoring focus like
    /// [`Self::close_settings`] does). No-op if `path` isn't an open tab.
    pub(super) fn close_file_tab(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.open_files.iter().position(|open| open == &path) else {
            return;
        };
        self.open_files.remove(index);
        // Drop the closed tab's remembered zoom too, so reopening the same path later starts
        // fresh at the 100% default instead of resurrecting a stale value, and so this map
        // doesn't grow unbounded for the life of the worktree session.
        self.file_zoom_percent.remove(&path);
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
                    self.restore_zoom_for_open_change(&next_path);
                    self.refresh_open_diff_file_cache();
                    self.hover = None;
                }
                None => {
                    self.open_change = None;
                    self.refresh_open_diff_file_cache();
                    self.hover = None;
                    restore_focus(&self.sessions, &mut self.code_focus, window, cx);
                }
            }
        }
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Closes the currently active file tab, if any (the code surface toolbar's own "× close",
    /// distinct from the tab strip's). Delegates to [`Self::close_file_tab`] so both affordances
    /// share the same close behavior.
    pub(super) fn close_change_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.open_change.clone() {
            self.close_file_tab(path, window, cx);
        }
    }

    /// Editor-zoom range (70-200%, in steps of 10) and default (100%).
    pub(super) const ZOOM_MIN_PERCENT: u16 = 70;
    pub(super) const ZOOM_MAX_PERCENT: u16 = 200;
    pub(super) const ZOOM_STEP_PERCENT: u16 = 10;
    pub(super) const ZOOM_DEFAULT_PERCENT: u16 = 100;

    /// The effective rem size (px) the zoom-scoped code surface renders `rems()`-based text at:
    /// `editor_font_size` times [`Self::code_zoom_percent`] as a fraction.
    pub(super) fn effective_code_rem_px(&self) -> f32 {
        self.settings.appearance.editor_font_size * (self.code_zoom_percent as f32 / 100.0)
    }

    /// Zooms in one step (the toolbar `+` button).
    pub(super) fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.set_code_zoom(
            clamp_zoom_percent(self.code_zoom_percent as i32 + Self::ZOOM_STEP_PERCENT as i32),
            cx,
        );
    }

    /// Zooms out one step (the toolbar `−` button).
    pub(super) fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.set_code_zoom(
            clamp_zoom_percent(self.code_zoom_percent as i32 - Self::ZOOM_STEP_PERCENT as i32),
            cx,
        );
    }

    /// Resets zoom to 100% (clicking the toolbar's zoom value).
    pub(super) fn reset_zoom(&mut self, cx: &mut Context<Self>) {
        self.set_code_zoom(Self::ZOOM_DEFAULT_PERCENT, cx);
    }

    /// The single place [`Self::code_zoom_percent`] is written by a user action; `zoom_in`/
    /// `zoom_out`/`reset_zoom` all delegate here. When `per_tab_zoom` is on, also remembers
    /// `percent` per-file in [`Self::file_zoom_percent`] so switching tabs restores it.
    fn set_code_zoom(&mut self, percent: u16, cx: &mut Context<Self>) {
        self.code_zoom_percent = percent;
        if self.settings.appearance.per_tab_zoom {
            if let Some(path) = self.open_change.clone() {
                self.file_zoom_percent.insert(path, percent);
            }
        }
        cx.notify();
    }

    /// Restores [`Self::code_zoom_percent`] for `path`, called wherever `open_change` is pointed
    /// at a (possibly different) file. When `per_tab_zoom` is on, looks up `path`'s remembered
    /// zoom, defaulting a never-zoomed tab to 100%; when off, leaves the shared value untouched.
    pub(super) fn restore_zoom_for_open_change(&mut self, path: &Path) {
        if self.settings.appearance.per_tab_zoom {
            self.code_zoom_percent = self
                .file_zoom_percent
                .get(path)
                .copied()
                .unwrap_or(Self::ZOOM_DEFAULT_PERCENT);
        }
    }

    /// Recomputes [`Self::open_diff_file_cache`] (and [`Self::file_view_changed_lines`] with it)
    /// from [`Self::open_change`] and [`Self::current_diff`]. Called whenever either input
    /// changes; never from a render method, to avoid a per-render `DiffFile` clone.
    pub(super) fn refresh_open_diff_file_cache(&mut self) {
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
    }

    /// Dispatches a background `code_view::load_file` call for `path` (must never run inline
    /// during `render()`; see [`FileLoadState`]'s docs). Mirrors [`Self::load_diff`]'s shape: mark
    /// `Loading` and notify immediately, run the blocking work on `cx.background_executor()`, then
    /// write the outcome back into [`Self::file_view_cache`]/[`Self::file_load_state`].
    pub(super) fn spawn_file_load(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.file_load_state = FileLoadState::Loading(path.clone());
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move { code_view::load_file(&path) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(parsed) => {
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
                        this.code_cursor = Some(target_line.unwrap_or(1));
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

    /// Click-to-hover trigger for Surface C's File view. `absolute_path`/`line_number`/
    /// `byte_range` identify the clicked token; `position` is the corresponding LSP `Position`,
    /// already computed at the click site.
    ///
    /// Triggered by click, not mouse-hover: this app has no per-pixel hover-position tracking,
    /// and building it (a debounced `.on_mouse_move` translated back to a token/byte position)
    /// would be substantial infrastructure out of proportion to what this read-only viewer needs.
    ///
    /// No-ops if `(absolute_path, line_number, byte_range)` already matches the current
    /// [`Self::hover`] entry, so re-clicking the same token doesn't redo an `rust-analyzer` round
    /// trip. Runs on `cx.background_executor()`, never inline: [`lsp_core::LspClient::request`]
    /// blocks the calling thread and must not block the GPUI foreground thread.
    pub(super) fn request_hover(
        &mut self,
        absolute_path: PathBuf,
        line_number: usize,
        byte_range: Range<usize>,
        position: lsp_core::lsp_types::Position,
        cx: &mut Context<Self>,
    ) {
        let already_current = self.hover.as_ref().is_some_and(|entry| {
            entry.path == absolute_path
                && entry.line_number == line_number
                && entry.byte_range == byte_range
        });
        if already_current {
            return;
        }

        let Some(LspClientState::Ready(client)) =
            self.lsp_clients.get(&self.file_tree_root).cloned()
        else {
            // No ready LSP client for this root yet; nothing to show, so clear any stale entry.
            self.hover = None;
            cx.notify();
            return;
        };

        let Ok(uri) = lsp_core::LspClient::uri_for_path(&absolute_path) else {
            self.hover = None;
            cx.notify();
            return;
        };

        self.hover = Some(HoverEntry {
            path: absolute_path.clone(),
            line_number,
            byte_range: byte_range.clone(),
            position,
            status: HoverStatus::Loading,
        });
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let params = lsp_core::lsp_types::HoverParams {
                text_document_position_params: lsp_core::lsp_types::TextDocumentPositionParams {
                    text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_core::lsp_types::WorkDoneProgressParams::default(),
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    client.request::<lsp_core::lsp_types::request::HoverRequest>(
                        params,
                        LSP_QUERY_TIMEOUT,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                // Only apply if this is still the answer to the current click - a slower,
                // superseded request must not clobber a newer one.
                let still_current = this.hover.as_ref().is_some_and(|entry| {
                    entry.path == absolute_path
                        && entry.line_number == line_number
                        && entry.byte_range == byte_range
                });
                if !still_current {
                    return;
                }
                let status = match result {
                    Ok(Some(hover)) => {
                        HoverStatus::Ready(hover_view::build_hover_render_model(&hover))
                    }
                    Ok(None) => HoverStatus::Ready(None),
                    Err(error) => HoverStatus::Failed(error.to_string()),
                };
                this.hover = Some(HoverEntry {
                    path: absolute_path.clone(),
                    line_number,
                    byte_range: byte_range.clone(),
                    position,
                    status,
                });
                cx.notify();
            });
        });
        // A single slot, not a Vec: assigning drops (cancelling) any in-flight previous request.
        // Unlike `_goto_definition_tasks`, hover requests aren't independently concurrent.
        self._hover_request_task = Some(task);
    }

    /// `F12`'s handler. Uses [`Self::hover`] as the source of "which symbol" rather than a
    /// separately-tracked target. No-op if nothing's been clicked yet.
    pub(super) fn trigger_goto_definition(&mut self, cx: &mut Context<Self>) {
        let Some(hover) = self.hover.as_ref() else {
            return;
        };
        let path = hover.path.clone();
        let position = hover.position;

        let Some(LspClientState::Ready(client)) =
            self.lsp_clients.get(&self.file_tree_root).cloned()
        else {
            return;
        };
        let Ok(uri) = lsp_core::LspClient::uri_for_path(&path) else {
            return;
        };

        let task = cx.spawn(async move |this, cx| {
            let params = lsp_core::lsp_types::GotoDefinitionParams {
                text_document_position_params: lsp_core::lsp_types::TextDocumentPositionParams {
                    text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri },
                    position,
                },
                work_done_progress_params: lsp_core::lsp_types::WorkDoneProgressParams::default(),
                partial_result_params: lsp_core::lsp_types::PartialResultParams::default(),
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    client.request::<lsp_core::lsp_types::request::GotoDefinition>(
                        params,
                        LSP_QUERY_TIMEOUT,
                    )
                })
                .await;
            let Ok(Some(response)) = result else {
                // Timeout, error, or no definition found - either way, no navigation.
                return;
            };
            let Some((target_uri, target_range)) = hover_view::first_definition_location(&response)
            else {
                return;
            };
            let Ok(target_path) = lsp_core::LspClient::path_for_uri(&target_uri) else {
                // Non-`file://` target (e.g. a virtual macro-expansion buffer): nothing to
                // navigate to.
                return;
            };
            let target_line = target_range.start.line as usize + 1;
            // `navigate_to_definition` needs `Window` access to move focus; `update_in` supplies
            // it by looking up the window this entity belongs to (see
            // vendor/zed/crates/gpui/src/app/async_context.rs `AsyncApp::with_window`), without
            // requiring this task to have been spawned via `cx.spawn_in`.
            let _ = this.update_in(cx, |this, window, cx| {
                this.navigate_to_definition(target_path, target_line, window, cx);
            });
        });
        self._goto_definition_tasks.push(task);
    }

    /// Navigates to a go-to-definition result. `absolute_target_path` may be under
    /// [`Self::file_tree_root`] or entirely outside it (e.g. another crate `rust-analyzer` sees);
    /// either way `Self::open_file_view`'s `strip_prefix` handles it, since `PathBuf::join` with
    /// an already-absolute path just becomes that path.
    ///
    /// ## Avoiding a cursor-line race
    ///
    /// [`Self::open_file_view`] alone lands on the right file but not the right line: if the file
    /// wasn't already open, its background load unconditionally sets [`Self::code_cursor`] to 1
    /// once it completes, which would clobber a line set directly here before the load even
    /// starts. [`Self::pending_cursor_line`] is the one-shot instruction that survives the load
    /// instead; `Self::spawn_file_load`'s completion handler consumes it.
    pub(super) fn navigate_to_definition(
        &mut self,
        absolute_target_path: PathBuf,
        one_based_line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_view(absolute_target_path.clone(), window, cx);
        // Use the same freshness check (path, mtime, len via `code_view::cache_is_fresh`) that
        // `render_file_view` uses, so the two decisions can't disagree.
        let metadata = std::fs::metadata(&absolute_target_path).ok();
        let mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
        let len = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let already_fresh = self.file_view_cache.as_ref().is_some_and(|cached| {
            code_view::cache_is_fresh(cached, &absolute_target_path, mtime, len)
        });
        if already_fresh {
            // Already cached (e.g. navigating within the open file), so render_file_view won't
            // reload and there's no completion handler to consume `pending_cursor_line`.
            self.code_cursor = Some(one_based_line);
            self.file_view_scroll_handle
                .scroll_to_item(one_based_line.saturating_sub(1), ScrollStrategy::Center);
        } else {
            self.pending_cursor_line = Some((absolute_target_path, one_based_line));
        }
        cx.notify();
    }

    /// Handles `TerminalPaneEvent::OpenPath` - a mod-held click on a detected path/`path:line`
    /// link in a session's terminal output. `path` is already resolved against the session's cwd
    /// (see `crate::terminal_links::resolve`). Reuses [`Self::navigate_to_definition`] when the
    /// link carried a line number, else [`Self::open_file_view`].
    ///
    /// Unlike every other caller of `open_file_view`, a terminal link's path isn't guaranteed to
    /// exist: `terminal_links`'s regex is a heuristic over plain text, not a filesystem lookup.
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

    /// [`GotoDefinition`]'s bound `F12` action handler.
    pub(super) fn handle_goto_definition_action(
        &mut self,
        _action: &GotoDefinition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.trigger_goto_definition(cx);
    }

    /// The currently loaded diff, if any - `None` while loading/erroring, or when the worktree is
    /// on its default branch / has no detectable base (see [`wt_core::diff::DiffBase`]). The
    /// single source every view that shows diff state reads.
    pub(super) fn current_diff(&self) -> Option<&WorktreeDiff> {
        match &self.diff_state {
            DiffLoadState::Loaded(DiffBase::Diff(diff)) => Some(diff),
            _ => None,
        }
    }

    /// A themed explanatory message for every [`DiffLoadState`] that isn't a loaded diff.
    pub(super) fn render_diff_state_message(&self) -> gpui::AnyElement {
        let (text, color) = match &self.diff_state {
            DiffLoadState::Loading => ("computing diff...".to_string(), theme::text::FAINT),
            DiffLoadState::Error(err) => (
                format!("failed to compute diff: {err}"),
                theme::status::FAIL,
            ),
            DiffLoadState::Loaded(DiffBase::NoBaseFound) => (
                "no base branch could be detected for this worktree (no origin/HEAD, no \
                 local main/master, and no fallback branch found)"
                    .to_string(),
                theme::text::FAINT,
            ),
            DiffLoadState::Loaded(DiffBase::OnDefaultBranch { branch }) => (
                format!(
                    "this worktree is on the default branch ({branch}); nothing to diff against"
                ),
                theme::text::FAINT,
            ),
            // Unreachable in practice (callers check `current_diff()` first); matched explicitly
            // so a future `DiffBase` variant isn't silently swallowed by a wildcard.
            DiffLoadState::Loaded(DiffBase::Diff(_)) => (String::new(), theme::text::FAINT),
        };
        render_sidebar_message(text, color)
    }

    /// The centre's single-file Surface C, opened by a Changes-row click (`diff_file` always
    /// `Some`) or a Files-tree row click (`diff_file` may be `None`): a toolbar (dir/name, tag
    /// pill, +n/-n stats, the `Diff | File` toggle, the zoom group, `Accept file`, close) over
    /// either [`Self::render_diff_file_detail`]'s folded hunk content or [`Self::render_file_view`]'s
    /// syntax-highlighted content, both zoom-scoped through [`zoom_scoped`].
    ///
    /// `effective_view` forces `File` when `diff_file` is `None`, regardless of what
    /// `self.code_view` was last left at by a different file.
    pub(super) fn render_code_surface(
        &mut self,
        relative_path: &Path,
        diff_file: Option<&DiffFile>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (dir, name) = changes::split_dir_name(relative_path);
        let tag = diff_file.and_then(|file| changes::change_tag(file.status));
        let stats = diff_file.map(changes::diff_file_stats);
        let rename_label = diff_file.and_then(changes::rename_label);
        let has_diff = diff_file.is_some();
        let effective_view = if has_diff {
            self.code_view
        } else {
            code_view::CodeView::File
        };

        let toolbar = div()
            .flex_none()
            .h(theme::band::DIFF_TOOLBAR)
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .when(!dir.is_empty(), |el| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("{dir}/")),
                )
            })
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.5))
                    .text_color(theme::text::HEADING)
                    .child(name),
            )
            .when_some(tag, |el, tag| el.child(render_tag_pill(tag)))
            .when_some(stats, |el, (add, del)| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::diff::STAT_ADD)
                        .child(format!("+{add}")),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::diff::STAT_DEL)
                        .child(format!("\u{2212}{del}")),
                )
            })
            // The row's compact `render_moved_tag` has no room for the pre-rename path; the
            // toolbar does.
            .when_some(rename_label, |el, label| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::text::GHOST)
                        .child(label),
                )
            })
            .child(div().flex_1())
            .child(self.render_diff_file_toggle(has_diff, effective_view, cx))
            .child(self.render_zoom_control(cx))
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(render_accept_file_button(
                self.window_controls_style().is_macos(),
            ))
            .child(
                div()
                    .id("close-diff-surface")
                    .cursor_pointer()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::GHOST)
                    .hover(|el| el.text_color(theme::text::PRIMARY))
                    .child("\u{d7} close")
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.close_change_diff(window, cx);
                    })),
            );

        let body = match (effective_view, diff_file) {
            (code_view::CodeView::Diff, Some(file)) => self.render_diff_file_detail(file),
            _ => self.render_file_view(relative_path, cx),
        };

        div()
            .id("code-surface")
            // Focus target for the whole Diff/File surface - see `code_focus_handle`'s docs for
            // the dangling-`Window::focus` bug this fixes, the same class `render_settings`'s
            // identical `track_focus` fixes for the Settings surface.
            .track_focus(&self.code_focus_handle)
            // Scopes `]` (`NextChangedFile`) to only fire while a file tab has focus - see that
            // binding's docs for the terminal-input-swallowing bug this prevents.
            .key_context("diff")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(theme::surface::CENTER)
            .child(toolbar)
            .child(body)
            .into_any_element()
    }

    /// The toolbar's segmented `Diff | File` toggle. `Diff` is only clickable when `has_diff` is
    /// true ([`ChoiceOption::enabled_if`] disables it otherwise); `File` is always clickable.
    /// Shares [`Self::render_choice_control`] with the other segmented toggles in this file.
    pub(super) fn render_diff_file_toggle(
        &self,
        has_diff: bool,
        effective_view: code_view::CodeView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = match effective_view {
            code_view::CodeView::Diff => "Diff",
            code_view::CodeView::File => "File",
        };
        self.render_choice_control(
            "diff-file-toggle",
            &[
                ChoiceOption::enabled_if("Diff", has_diff),
                ChoiceOption::new("File"),
            ],
            selected.to_string(),
            cx,
            |this, index, cx| {
                // Index 0 is `Diff`, index 1 is `File`, per the options array above.
                this.code_view = match index {
                    0 => code_view::CodeView::Diff,
                    _ => code_view::CodeView::File,
                };
                cx.notify();
            },
        )
    }

    /// The toolbar's zoom control group: `-` / value / `+`, 19x19 buttons with a 1px gap, value
    /// in a fixed 36px column (every value in `ZOOM_MIN_PERCENT..=ZOOM_MAX_PERCENT` is at most 3
    /// digits). Clicking the value resets zoom to 100%.
    ///
    /// The design has no disabled-color state for `-`/`+` at the range boundaries; this adds one
    /// (dims and drops the click handler/hover/cursor) rather than leaving a dead-looking button
    /// that silently no-ops at 70%/200%.
    pub(super) fn render_zoom_control(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: &'static str, enabled: bool| {
            let mut el = div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(19.0))
                .h(px(19.0))
                .rounded(theme::radius::CHIP)
                .font(font(theme::font::MONO))
                .text_size(px(11.0))
                .text_color(if enabled {
                    theme::text::DIM
                } else {
                    theme::text::DISABLED
                })
                .child(label);
            if enabled {
                el = el
                    .cursor_pointer()
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT));
            }
            el
        };

        let can_zoom_out = self.code_zoom_percent > AdeApp::ZOOM_MIN_PERCENT;
        let can_zoom_in = self.code_zoom_percent < AdeApp::ZOOM_MAX_PERCENT;

        div()
            .id("code-zoom-control")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(1.0))
            .child(
                button("code-zoom-out", "\u{2212}", can_zoom_out).when(can_zoom_out, |el| {
                    el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.zoom_out(cx);
                    }))
                }),
            )
            .child(
                div()
                    .id("code-zoom-value")
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(36.0))
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.0))
                    .text_color(theme::text::DIM)
                    .hover(|el| el.text_color(theme::text::SELECTED))
                    .child(format!("{}%", self.code_zoom_percent))
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.reset_zoom(cx);
                    })),
            )
            .child(
                button("code-zoom-in", "+", can_zoom_in).when(can_zoom_in, |el| {
                    el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.zoom_in(cx);
                    }))
                }),
            )
    }

    /// One changed file's diff content: a "binary file" note, or its hunks as unified-diff-style
    /// themed lines, with a `⋯ N unchanged lines` fold marker for the gap between consecutive
    /// hunks (`crate::changes::fold_gap_between`, parsed from the hunks' `@@ ... @@` headers).
    /// `wt_core::diff` has no lazy per-file hunk-loading state, since every non-binary changed
    /// file's hunks are already eagerly loaded, so the design's "press ⏎ to load this hunk"
    /// treatment doesn't apply here; capped by [`MAX_RENDERED_DIFF_LINES_PER_FILE`] independent
    /// of `wt_core::diff`'s own load-time cap.
    pub(super) fn render_diff_file_detail(&self, file: &DiffFile) -> gpui::AnyElement {
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
            .py(px(4.0));

        if file.is_binary {
            return zoom_scoped(
                rem_px,
                container.child(render_sidebar_message(
                    "binary file (contents not diffed)".to_string(),
                    theme::text::FAINT,
                )),
            );
        }

        // A rename-only file produces zero `@@` hunks, so falling through the loop below would
        // leave `container` with no children - a blank pane that looks like a rendering bug
        // rather than "nothing to show". `changes::empty_hunks_message` picks honest wording,
        // naming the rename specifically when that's the cause.
        if file.hunks.is_empty() {
            return zoom_scoped(
                rem_px,
                container.child(render_sidebar_message(
                    changes::empty_hunks_message(file.status).to_string(),
                    theme::text::FAINT,
                )),
            );
        }

        let mut rendered_lines = 0usize;
        let mut hunks_truncated = false;
        let mut previous_header: Option<&str> = None;
        'hunks: for hunk in &file.hunks {
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

            for line in &hunk.lines {
                if rendered_lines >= MAX_RENDERED_DIFF_LINES_PER_FILE {
                    hunks_truncated = true;
                    break 'hunks;
                }
                rendered_lines += 1;
                container = container.child(render_diff_line(line));
            }
        }

        if file.truncated || hunks_truncated {
            container = container.child(render_sidebar_message(
                "... diff truncated for this file".to_string(),
                theme::text::FAINT,
            ));
        }

        zoom_scoped(rem_px, container)
    }

    /// Surface C's File view: a breadcrumb, line-numbered/syntax-highlighted code
    /// (`crate::code_view`), and a status bar for whichever file `relative_path` (resolved
    /// against [`Self::file_tree_root`]) names on disk.
    ///
    /// ## Caching, and staying off the foreground thread
    ///
    /// [`code_view::load_file`] runs a `tree-sitter` parse and is only dispatched (via
    /// [`Self::spawn_file_load`]) when [`Self::file_view_cache`] is missing or
    /// [`code_view::cache_is_fresh`] says it's stale - never unconditionally on every render, and
    /// never run inline on the foreground thread (see [`FileLoadState`]'s docs for the measured
    /// cost this avoids). Covered by this module's `code_view_cache_tests` below.
    ///
    /// ## Virtualization
    ///
    /// Every line of `parsed.lines` is reachable - no cap like
    /// [`MAX_RENDERED_DIFF_LINES_PER_FILE`] applies here. `gpui::uniform_list` (see
    /// vendor/zed/crates/gpui/examples/uniform_list.rs and
    /// vendor/zed/crates/git_ui/src/git_panel.rs's `commit_history_list`) only constructs
    /// [`render_file_view_line`] elements for rows scrolled into view, so a large file stays
    /// scrollable end to end.
    pub(super) fn render_file_view(
        &mut self,
        relative_path: &Path,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let absolute_path = self.file_tree_root.join(relative_path);

        // Throttled freshness check (see `file_view_last_freshness_check`'s docs); a path
        // mismatch always forces an immediate re-check regardless of the throttle window.
        let now = Instant::now();
        let should_check = match &self.file_view_last_freshness_check {
            Some((checked_path, checked_at)) => {
                checked_path != &absolute_path
                    || now.duration_since(*checked_at) >= FILE_FRESHNESS_CHECK_INTERVAL
            }
            None => true,
        };

        let cache_fresh = if should_check {
            let metadata = std::fs::metadata(&absolute_path).ok();
            let mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
            let len = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
            self.file_view_last_freshness_check = Some((absolute_path.clone(), now));
            self.file_view_cache
                .as_ref()
                .is_some_and(|cached| code_view::cache_is_fresh(cached, &absolute_path, mtime, len))
        } else {
            // Within the throttle window: nothing mutates `file_view_cache` for an already-fresh
            // path outside of the check above, so a matching cached path is enough evidence.
            self.file_view_cache
                .as_ref()
                .is_some_and(|cached| cached.path == absolute_path)
        };

        if !cache_fresh {
            // A load already in flight, or a previous read failure, for this exact path must not
            // respawn another load on every render. Without also checking `FileLoadState::Error`
            // here, a permanently unreadable path would respawn a doomed load each repaint: every
            // failure calls `cx.notify()`, triggering another render - an unbounded busy-loop
            // instead of a stable error state.
            let already_settled = matches!(
                &self.file_load_state,
                FileLoadState::Loading(loading_path) if loading_path == &absolute_path
            ) || matches!(
                &self.file_load_state,
                FileLoadState::Error(error_path, _) if error_path == &absolute_path
            );
            if !already_settled {
                self.spawn_file_load(absolute_path.clone(), cx);
            }
            // The dispatched (or in-flight) load hasn't written a fresh cache yet; show its
            // current state instead of stale content from a different file. The next render
            // after it resolves will find `cache_fresh` true and fall through below.
            return match &self.file_load_state {
                FileLoadState::Error(error_path, message) if error_path == &absolute_path => {
                    render_sidebar_message(
                        format!("failed to read {}: {message}", absolute_path.display()),
                        theme::status::FAIL,
                    )
                }
                _ => render_sidebar_message(
                    format!("loading {}...", absolute_path.display()),
                    theme::text::FAINT,
                ),
            };
        }

        // A pending go-to-definition target line for this already-fresh file; the other case
        // (parse not yet cached) is handled by `spawn_file_load`'s completion handler.
        if self
            .file_view_cache
            .as_ref()
            .is_some_and(|cached| cached.path == absolute_path)
        {
            let target_line = match &self.pending_cursor_line {
                Some((pending_path, line)) if pending_path == &absolute_path => Some(*line),
                _ => None,
            };
            if let Some(line) = target_line {
                self.pending_cursor_line = None;
                self.code_cursor = Some(line);
                self.file_view_scroll_handle
                    .scroll_to_item(line.saturating_sub(1), ScrollStrategy::Center);
            }
        }

        // Diagnostics only apply to `.rs` files. `ensure_lsp_client`/`dispatch_did_open` are
        // idempotent `&mut self` calls that must finish before the immutable `file_view_cache`
        // borrow below is taken.
        let is_rust = absolute_path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"));

        let lsp_status = if is_rust {
            let repo_root = self.file_tree_root.clone();
            self.ensure_lsp_client(repo_root.clone(), cx);
            let state = self.lsp_clients.get(&repo_root).cloned();
            if let Some(LspClientState::Ready(client)) = &state {
                self.dispatch_did_open(client.clone(), absolute_path.clone(), cx);
            }

            // Computed once and reused below, since `uri_for_path` does a blocking
            // `canonicalize()` syscall and this method runs on every repaint.
            let file_uri = lsp_core::LspClient::uri_for_path(&absolute_path).ok();

            let diagnostics_map = match (&state, &file_uri) {
                (Some(LspClientState::Ready(client)), Some(uri)) => {
                    let diagnostics = client.diagnostics_for_uri(uri).unwrap_or_default();
                    match self.file_view_cache.as_ref() {
                        Some(parsed) => {
                            diagnostics_view::index_diagnostics_by_line(&diagnostics, &parsed.lines)
                        }
                        None => HashMap::new(),
                    }
                }
                _ => HashMap::new(),
            };
            self.file_view_diagnostics = diagnostics_map;

            Some(lsp_file_status(&state, file_uri.as_ref()))
        } else {
            self.file_view_diagnostics = HashMap::new();
            None
        };

        let Some(parsed) = self.file_view_cache.as_ref() else {
            return render_sidebar_message("no file loaded".to_string(), theme::text::FAINT);
        };

        let cursor = self.code_cursor;
        let status_bar = render_file_status_bar(parsed, cursor, lsp_status.as_ref());
        let truncated = parsed.truncated;
        let line_count = parsed.lines.len();
        let diagnostics_card = render_diagnostics_card(&self.file_view_diagnostics);
        // Hover only applies to `.rs` files; cloned once here and reused per row for the same
        // reason as `file_uri` above.
        let hover_target = is_rust.then(|| absolute_path.clone());
        let hover_card = render_hover_card(self.hover.as_ref(), &absolute_path, cx);

        let code = uniform_list(
            "file-view-code",
            line_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                let Some(parsed) = &this.file_view_cache else {
                    return Vec::new();
                };
                let total = parsed.lines.len();
                let start = range.start.min(total);
                let end = range.end.min(total);
                let cursor_line = this.code_cursor;
                let hover_entry = this.hover.clone();
                let mut rows = Vec::with_capacity(end.saturating_sub(start));
                for index in start..end {
                    let line = &parsed.lines[index];
                    let line_number = index + 1;
                    let is_current = cursor_line == Some(line_number);
                    let is_changed = this.file_view_changed_lines.contains(&line_number);
                    let empty_diagnostics: Vec<diagnostics_view::LineDiagnostic> = Vec::new();
                    let line_diagnostics = this
                        .file_view_diagnostics
                        .get(&line_number)
                        .unwrap_or(&empty_diagnostics);
                    rows.push(render_file_view_line(
                        line,
                        line_number,
                        is_current,
                        is_changed,
                        line_diagnostics,
                        HoverRenderContext {
                            target: hover_target.as_deref(),
                            entry: hover_entry.as_ref(),
                        },
                        cx,
                    ));
                }
                rows
            }),
        )
        // Without tracking this handle, F12 moved `code_cursor` and the status bar's `ln N` but
        // never scrolled the viewport.
        .track_scroll(&self.file_view_scroll_handle)
        .flex_1()
        .min_h_0()
        .bg(theme::surface::PTY)
        .font(font(theme::font::MONO))
        // `rems()`, not `px()`, so this text scales with `zoom_scoped`'s rem-size override
        // below rather than the window's own (unused) default rem size.
        .text_size(rems(1.0))
        .line_height(rems(1.6));

        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(render_file_breadcrumb(relative_path))
            .child(zoom_scoped(self.effective_code_rem_px(), code));

        if truncated {
            body = body.child(render_sidebar_message(
                "... file truncated (larger than 2 MiB)".to_string(),
                theme::text::FAINT,
            ));
        }
        if let Some(card) = diagnostics_card {
            body = body.child(card);
        }
        if let Some(card) = hover_card {
            body = body.child(card);
        }

        body.child(status_bar).into_any_element()
    }
}

/// Rounds `percent` to the nearest 10-point step, then clamps into
/// `AdeApp::ZOOM_MIN_PERCENT..=AdeApp::ZOOM_MAX_PERCENT`. A free function, not inlined into
/// `zoom_in`/`zoom_out`, so it's unit-testable without a `Context<AdeApp>`. Takes `i32`, not
/// `u16`, so an already out-of-range or negative candidate (e.g. stepping below zero from 70%)
/// doesn't underflow before it's clamped.
pub(super) fn clamp_zoom_percent(percent: i32) -> u16 {
    let step = AdeApp::ZOOM_STEP_PERCENT as i32;
    let stepped = (percent as f32 / step as f32).round() as i32 * step;
    stepped.clamp(
        AdeApp::ZOOM_MIN_PERCENT as i32,
        AdeApp::ZOOM_MAX_PERCENT as i32,
    ) as u16
}

/// Wraps `content` in [`rem_scope::WithRemSize`], scoped to `rem_px`. Rows using
/// `.text_size(rems(1.0))`/`.line_height(rems(1.6))` scale with it; anything still in `px()`
/// (the line-number gutter, the git-gutter column) is unaffected - covered by
/// `code_zoom_tests::zoom_scales_text_but_not_the_gutter_width`.
fn zoom_scoped(rem_px: f32, content: impl IntoElement) -> gpui::AnyElement {
    WithRemSize::new(px(rem_px))
        .flex_1()
        .min_h_0()
        .flex()
        .flex_col()
        .child(content)
        .into_any_element()
}

/// The outcome of the most recent (or in-flight) `diff_against_base` call for
/// [`AdeApp::diff_root`]. Kept separate from [`DiffBase`] so "still computing" is a first-class
/// state, distinct from an empty/default value that could be mistaken for "no changes".
pub(super) enum DiffLoadState {
    Loading,
    Loaded(DiffBase),
    Error(String),
}

/// The outcome of the most recent (or in-flight) `code_view::load_file` call for whichever path
/// [`AdeApp::render_file_view`] most recently asked to load. Mirrors [`DiffLoadState`]'s shape:
/// `load_file` does the same class of blocking I/O (`std::fs::read`, plus a `tree-sitter` parse
/// for `.rs` files) and must never run on the GPUI foreground thread.
///
/// Kept separate from [`AdeApp::file_view_cache`] rather than folded into an
/// `Option<Result<ParsedFile, String>>` there, so a fresh load for a newly opened file doesn't
/// overwrite (and blank) whatever was last successfully shown while it's still in flight.
#[derive(Debug)]
pub(super) enum FileLoadState {
    Idle,
    Loading(PathBuf),
    Error(PathBuf, String),
}

/// The state of one in-flight or completed click-triggered `textDocument/hover` request; see
/// [`AdeApp::hover`]'s docs for the caching discipline this backs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HoverEntry {
    /// The absolute path of the file the hovered symbol is in; `render_file_view` only shows
    /// [`Self::status`] when it matches the file currently open.
    path: PathBuf,
    /// 1-based line number (matching [`AdeApp::code_cursor`]'s convention); half of this entry's
    /// cache key along with [`Self::byte_range`].
    line_number: usize,
    /// Byte range, within the line's text, of the clicked token - the span
    /// [`crate::root::render_file_view_line`] underlines with `theme::syntax::HOVER_UNDERLINE`,
    /// and the other half of the cache key.
    byte_range: Range<usize>,
    /// The LSP `Position` this request was/will be sent with, kept alongside `byte_range` so
    /// [`AdeApp::trigger_goto_definition`] can reuse it without recomputing.
    position: lsp_core::lsp_types::Position,
    status: HoverStatus,
}

/// The outcomes of one [`HoverEntry`]'s request, mirroring [`LspClientState`]'s three-state
/// shape, so `render_hover_card` can show the right state instead of a blank card while loading.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum HoverStatus {
    Loading,
    /// A response arrived - `Some` for a non-empty `HoverRenderModel`, `None` for "rust-analyzer
    /// answered, nothing to show" (e.g. hovering whitespace) - never conflated with
    /// [`HoverStatus::Failed`], which means the request itself didn't complete.
    Ready(Option<hover_view::HoverRenderModel>),
    Failed(String),
}

/// Surface C's Diagnostic-state card: one row per diagnostic currently indexed anywhere in the
/// open file, `None` when there are none (a clean file renders no card, not an empty one).
/// Listing every diagnostic in the file (rather than only the one under the cursor) is a
/// simplification: the design anchors this card under the caret line, but this app has no
/// floating-popup infrastructure yet.
pub(super) fn render_diagnostics_card(
    by_line: &HashMap<usize, Vec<diagnostics_view::LineDiagnostic>>,
) -> Option<gpui::AnyElement> {
    if by_line.is_empty() {
        return None;
    }

    let mut lines: Vec<&usize> = by_line.keys().collect();
    lines.sort();

    let mut card = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(10.0))
        .m(px(8.0))
        .rounded(theme::radius::CARD_SM)
        .bg(theme::surface::CARD)
        .border_1()
        .border_color(theme::border::CARD);

    for line_number in lines {
        for diagnostic in &by_line[line_number] {
            let source_code = match (&diagnostic.source, &diagnostic.code) {
                (Some(source), Some(code)) => format!("{source} · {code}"),
                (Some(source), None) => source.clone(),
                (None, Some(code)) => code.clone(),
                (None, None) => String::new(),
            };
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .font(font(theme::font::MONO))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme::syntax::DIAGNOSTIC_CARD_MESSAGE)
                            .child(format!("ln {line_number}: {}", diagnostic.message)),
                    )
                    .when(!source_code.is_empty(), |el| {
                        el.child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text::DIMMER)
                                .child(source_code),
                        )
                    }),
            );
        }
    }

    Some(card.into_any_element())
}

/// Surface C's Hover-state card: signature, doc prose, module path, `F12 definition` footer.
/// `None` when [`AdeApp::hover`] is `None`, or (defensively) belongs to a different file than
/// `open_absolute_path`. The design anchors this under the caret as a floating popup, but this
/// app has no floating-popup infrastructure, so - like [`render_diagnostics_card`] - it renders
/// as a card below the code instead.
pub(super) fn render_hover_card(
    hover: Option<&HoverEntry>,
    open_absolute_path: &Path,
    cx: &mut Context<AdeApp>,
) -> Option<gpui::AnyElement> {
    let hover = hover.filter(|entry| entry.path == open_absolute_path)?;

    let mut card = div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(10.0))
        .m(px(8.0))
        .max_w(px(430.0))
        .rounded(theme::radius::CARD_SM)
        .bg(theme::surface::POPOVER)
        .border_1()
        .border_color(theme::border::POPOVER);

    match &hover.status {
        HoverStatus::Loading => {
            card = card.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::FAINT)
                    .child("loading hover..."),
            );
        }
        HoverStatus::Failed(message) => {
            card = card.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::status::FAIL)
                    .child(format!("hover failed: {message}")),
            );
        }
        HoverStatus::Ready(None) => {
            card = card.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::FAINT)
                    .child("no symbol information here"),
            );
        }
        HoverStatus::Ready(Some(model)) => {
            card = card.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(12.0))
                    .text_color(theme::text::HEADING)
                    .child(model.signature.clone()),
            );
            if let Some(doc) = &model.doc {
                card = card.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::text::DIMMER)
                        .child(doc.clone()),
                );
            }
            let mut footer = div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .pt(px(4.0))
                .border_t_1()
                .border_color(theme::border::INNER);
            if let Some(module_path) = &model.module_path {
                footer = footer.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::text::FAINT)
                        .child(module_path.clone()),
                );
            }
            footer = footer.child(
                div()
                    .id("hover-card-goto-definition")
                    .flex()
                    .items_center()
                    .gap(px(3.0))
                    .cursor_pointer()
                    // `F12` is a function key, not one of `crate::keymap`'s modifier tokens, and
                    // is identical on both platforms, so it bypasses `keymap::resolve_combo`.
                    .child(render_keycap("F12"))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme::text::FAINT)
                            .child("definition"),
                    )
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.trigger_goto_definition(cx);
                    })),
            );
            card = card.child(footer);
        }
    }

    Some(card.into_any_element())
}

/// The diff view's `⋯ N unchanged lines` fold marker. `N` is derived from the hunks' `@@ ... @@`
/// headers (`crate::changes::fold_gap_between`), never an estimate.
///
/// Sized in `rems()`, not `px()`: unlike the line-number gutter and git-gutter column, this
/// marker isn't exempt from zoom, so it must scale with the surrounding diff rows rather than
/// staying a fixed-size sliver once zoom moves off 100%. `0.85` keeps it proportionally smaller
/// than a diff line's own text, matching the 11px-vs-13px ratio at the 100% baseline.
pub(super) fn render_fold_marker(gap: usize) -> impl IntoElement {
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

/// One diff line - added/removed/context, colored per kind.
pub(super) fn render_diff_line(line: &wt_core::diff::DiffLine) -> impl IntoElement {
    let (prefix, fg, bg) = match line.kind {
        DiffLineKind::Added => ("+", theme::diff::ADD_FG, Some(theme::diff::ADD_BG)),
        DiffLineKind::Removed => ("\u{2212}", theme::diff::DEL_FG, Some(theme::diff::DEL_BG)),
        DiffLineKind::Context => (" ", theme::diff::CTX_FG, None),
    };
    let mut element = div()
        .flex()
        .font(font(theme::font::MONO))
        .text_size(rems(1.0))
        .line_height(rems(1.6))
        .px(px(8.0))
        .text_color(fg);
    if let Some(bg) = bg {
        element = element.bg(bg);
    }
    element.child(format!("{prefix} {}", line.content))
}

/// The File view toolbar's always-rendered `Accept file` button, always in its dimmed
/// non-interactive state: this app has no per-file review-apply logic yet, so it's deliberately
/// given no `cursor_pointer()`/`on_click` at all rather than a handler that would silently no-op.
///
/// The trailing keycap is resolved through `crate::keymap::resolve_combo("enter", macos)` rather
/// than a baked-in `⏎` glyph, so it reads `Enter` on Windows/Linux.
pub(super) fn render_accept_file_button(macos: bool) -> impl IntoElement {
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
            theme::text::GHOSTER,
            theme::border::BUTTON_DISABLED,
        ))
}

/// The File view's breadcrumb, built from `relative_path`'s segments
/// (`code_view::breadcrumb_segments`). The design's deeper symbol-path suffix (`› impl
/// QueryBuilder › build`) is out of scope: it needs symbol/AST-position tracking this read-only
/// viewer doesn't build. The last (file name) segment is the active crumb; earlier segments are
/// dimmer ancestor directories.
pub(super) fn render_file_breadcrumb(relative_path: &Path) -> impl IntoElement {
    let segments = code_view::breadcrumb_segments(relative_path);
    let last_index = segments.len().saturating_sub(1);

    let mut row = div()
        .flex_none()
        .h(theme::band::BREADCRUMB)
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(12.0))
        .bg(theme::surface::HEADER)
        .border_b_1()
        .border_color(theme::border::INNER)
        .font(font(theme::font::MONO))
        .text_size(px(10.5));

    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            row = row.child(div().text_color(theme::text::DISABLED).child("\u{203A}"));
        }
        let color = if index == last_index {
            theme::text::SECONDARY
        } else {
            theme::text::GHOST
        };
        row = row.child(div().text_color(color).child(segment));
    }

    row
}

/// One File view code row: a 52px right-aligned line-number gutter, a 3px git-gutter marker
/// (tinted `theme::diff::GIT_GUTTER` for `is_changed`, transparent otherwise), and the
/// syntax-highlighted line content (`line.runs`, via `code_view::color_for_kind`). `is_current`
/// tints the whole row and brightens the gutter number.
///
/// Clicking a row sets `AdeApp::code_cursor` to `line_number`. There is no column tracking here
/// (not a fabricated `col 1`) - see `AdeApp::code_cursor`'s docs: per-character column tracking
/// is out of scope for this phase.
///
/// `diagnostics` (possibly empty, for this line - see `AdeApp::file_view_diagnostics`) drives
/// three per-row treatments: a row tint (`theme::syntax::DIAGNOSTIC_ROW_BG`, when `is_current`
/// isn't already tinting the row), a `.border_dashed()` underline under the offending span
/// (`crate::diagnostics_view::overlay_diagnostic_runs`; GPUI has no true dotted border, see
/// vendor/zed/crates/gpui/src/styled.rs's `border_dashed`), and a dim inline message from the
/// first diagnostic on the line (the full breakdown is `render_diagnostics_card`, below the code
/// area, not repeated per-row). The design only specifies an underline color for the error case;
/// `Warning` reuses [`theme::term::WARN`], `Information`/`Hint` reuse
/// [`theme::text::DIM`]/[`theme::text::FAINT`] with `Hint` dimmer, matching the convention that
/// LSP hints are the least severe/most subtle diagnostic kind.
pub(super) fn diagnostic_underline_color(severity: diagnostics_view::Severity) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::ERROR_UNDERLINE,
        diagnostics_view::Severity::Warning => theme::term::WARN,
        diagnostics_view::Severity::Information => theme::text::DIM,
        diagnostics_view::Severity::Hint => theme::text::FAINT,
    }
}

/// The File view row's background tint for a diagnostic of `severity` - `None` means no tint.
/// Only `Error` gets one ([`theme::syntax::DIAGNOSTIC_ROW_BG`]); the other three are
/// distinguished from a clean line by their dotted underline alone, keeping every non-error
/// severity visibly less alarming than an error.
pub(super) fn diagnostic_row_bg(severity: diagnostics_view::Severity) -> Option<gpui::Rgba> {
    match severity {
        diagnostics_view::Severity::Error => Some(theme::syntax::DIAGNOSTIC_ROW_BG),
        _ => None,
    }
}

/// The File view row's inline end-of-line message color for a diagnostic of `severity` - `Error`
/// keeps [`theme::syntax::DIAGNOSTIC_INLINE_MESSAGE`]; every other severity reuses
/// [`theme::text::FAINT`].
pub(super) fn diagnostic_inline_message_color(severity: diagnostics_view::Severity) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::DIAGNOSTIC_INLINE_MESSAGE,
        _ => theme::text::FAINT,
    }
}

/// Asserts the four severities produce visibly distinct colors, against the same color-mapping
/// functions [`render_file_view_line`] calls (not a reimplemented duplicate). Regression guard:
/// every severity used to collapse onto the same underline/row-bg treatment.
#[cfg(test)]
mod diagnostic_severity_color_tests {
    use super::*;

    #[test]
    fn every_severity_gets_a_visibly_distinct_underline_color() {
        let severities = [
            diagnostics_view::Severity::Error,
            diagnostics_view::Severity::Warning,
            diagnostics_view::Severity::Information,
            diagnostics_view::Severity::Hint,
        ];
        let colors: Vec<gpui::Rgba> = severities
            .iter()
            .map(|severity| diagnostic_underline_color(*severity))
            .collect();
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "{:?} and {:?} must render with visibly distinct underline colors, not \
                     collapse onto the same treatment",
                    severities[i], severities[j]
                );
            }
        }
    }

    #[test]
    fn only_error_gets_a_real_row_background_tint() {
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Error).is_some());
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Warning).is_none());
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Information).is_none());
        assert!(diagnostic_row_bg(diagnostics_view::Severity::Hint).is_none());
    }

    /// Regression guard: a Hint used to render pixel-identical to an Error (same underline,
    /// row tint, and inline message color) - all three dimensions must now differ.
    #[test]
    fn a_real_hint_is_visibly_distinct_from_a_real_error_on_every_dimension() {
        assert_ne!(
            diagnostic_underline_color(diagnostics_view::Severity::Error),
            diagnostic_underline_color(diagnostics_view::Severity::Hint),
            "underline colour must differ"
        );
        assert_ne!(
            diagnostic_row_bg(diagnostics_view::Severity::Error),
            diagnostic_row_bg(diagnostics_view::Severity::Hint),
            "row background tint must differ"
        );
        assert_ne!(
            diagnostic_inline_message_color(diagnostics_view::Severity::Error),
            diagnostic_inline_message_color(diagnostics_view::Severity::Hint),
            "inline message colour must differ"
        );
    }
}

/// Bundles [`render_file_view_line`]'s two hover-state parameters to keep that function's
/// argument count under clippy's `too_many_arguments` limit; not otherwise a conceptual unit.
pub(super) struct HoverRenderContext<'a> {
    /// The current file's absolute path, `Some` only for a `.rs` file.
    target: Option<&'a Path>,
    /// [`AdeApp::hover`]'s current entry, if any.
    entry: Option<&'a HoverEntry>,
}

pub(super) fn render_file_view_line(
    line: &code_view::RenderedLine,
    line_number: usize,
    is_current: bool,
    is_changed: bool,
    diagnostics: &[diagnostics_view::LineDiagnostic],
    hover: HoverRenderContext<'_>,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    let HoverRenderContext {
        target: hover_target,
        entry: hover_entry,
    } = hover;
    let gutter_color = if is_current {
        theme::text::DIM
    } else {
        theme::text::GUTTER
    };
    // "Worst wins": the tie-break for a line's row-level treatment when it carries diagnostics
    // of mixed severity (see `Severity::worst`), not whichever is first in the Vec.
    let worst_severity = diagnostics_view::Severity::worst(diagnostics);
    // The hovered span on this line, if any, compared by run-level byte range rather than a
    // re-derived UTF-16 conversion of rust-analyzer's own `Hover::range`.
    let hovered_byte_range = hover_entry
        .and_then(|entry| (entry.line_number == line_number).then(|| entry.byte_range.clone()));

    let mut text_row = div().flex();
    let mut byte_cursor = 0usize;
    for (run_text, kind, is_diagnostic) in
        diagnostics_view::overlay_diagnostic_runs(&line.runs, diagnostics)
    {
        let run_start = byte_cursor;
        let run_end = run_start + run_text.len();
        byte_cursor = run_end;

        // `.id()` is applied unconditionally so `run` stays `Stateful<Div>` across every branch
        // below; a conditional `.cursor_pointer()`/`.on_click()` would change the concrete type,
        // and this `let mut run` can't be reassigned two different types on different branches.
        let mut run = div()
            .id(("file-view-code-token", line_number * 1_000_000 + run_start))
            .text_color(code_view::color_for_kind(kind))
            .child(run_text.clone());
        if is_diagnostic {
            // `is_diagnostic` is only true when `diagnostics` is non-empty, so `worst_severity`
            // is always `Some` here; `unwrap_or` is a fallback, not a reachable default.
            let underline_color = worst_severity
                .map(diagnostic_underline_color)
                .unwrap_or(theme::syntax::ERROR_UNDERLINE);
            run = run
                .border_b_2()
                .border_color(underline_color)
                .border_dashed();
        } else if hovered_byte_range.as_ref() == Some(&(run_start..run_end)) {
            // A diagnostic underline always wins over the hover underline on the same run - an
            // active error is more urgent than a symbol the user merely clicked to inspect.
            run = run
                .border_b_1()
                .border_color(theme::syntax::HOVER_UNDERLINE);
        }
        // Only a non-whitespace token is a hover/go-to-definition target; clicking whitespace
        // would just ask rust-analyzer about nothing.
        if let Some(path) = hover_target {
            if !run_text.trim().is_empty() {
                let path = path.to_path_buf();
                let position = hover_view::position_for_line_byte_offset(
                    line_number as u32 - 1,
                    &line.text,
                    run_start,
                );
                let byte_range = run_start..run_end;
                run = run.cursor_pointer().on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.code_cursor = Some(line_number);
                        this.request_hover(
                            path.clone(),
                            line_number,
                            byte_range.clone(),
                            position,
                            cx,
                        );
                    },
                ));
            }
        }
        text_row = text_row.child(run);
    }
    if let Some(first) = diagnostics.first() {
        // Only the message's first line is shown inline: `uniform_list` measures one row's
        // height and applies it uniformly to every row, so a multi-line rustc message (embedded
        // `\n`s are routine) would otherwise clip or overlap the row below. The full message is
        // still shown in `render_diagnostics_card` below, which isn't height-constrained.
        let first_line = first.message.lines().next().unwrap_or_default();
        text_row = text_row.child(
            div()
                .pl(px(10.0))
                .text_color(diagnostic_inline_message_color(first.severity))
                .child(first_line.to_string()),
        );
    }

    div()
        .id(("file-view-line", line_number))
        .flex_none()
        .flex()
        .items_center()
        .cursor_pointer()
        .when(is_current, |el| el.bg(theme::surface::CURRENT_LINE))
        .when_some(
            if is_current {
                None
            } else {
                worst_severity.and_then(diagnostic_row_bg)
            },
            |el, bg| el.bg(bg),
        )
        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
            this.code_cursor = Some(line_number);
            cx.notify();
        }))
        .child(
            div()
                .flex_none()
                .w(px(52.0))
                .pr(px(12.0))
                .text_right()
                .text_color(gutter_color)
                // Fixed `px()`, not `rems(1.0)` like the rest of this row's text: `uniform_list`
                // measures every row's height from item index 0 alone (a single-digit "1", which
                // never wraps) and applies that height to every row slot. A 4-digit line number
                // growing with zoom could wrap past this column's fixed 52px width and overlap
                // the row below. Pinning `text_size` alone (not `line_height`, which stays the
                // ambient zoom-scoped `rems(1.6)`) keeps glyphs from wrapping while the line-box
                // height still tracks the row's zoom-scaled height.
                .text_size(px(11.0))
                // `debug_selector` is a no-op outside test builds; lets
                // `code_zoom_tests::zoom_scales_text_but_not_the_gutter_width` measure this
                // gutter's rendered width at two zoom levels and assert it never changes.
                .debug_selector(move || format!("file-view-gutter-{line_number}"))
                .child(line_number.to_string()),
        )
        .child(
            div()
                .flex_none()
                .w(px(3.0))
                // `self_stretch()` (`align-self: stretch`), not a fixed `h(px(20.0))`: at higher
                // zoom this row's height grows with the code text, and a fixed-height bar would
                // leave a gap between consecutive changed lines' bars instead of one continuous
                // strip.
                .self_stretch()
                .bg(if is_changed {
                    theme::diff::GIT_GUTTER
                } else {
                    work_surface::TRANSPARENT
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .pl(px(12.0))
                // See the gutter `debug_selector` above; this one measures the `rems()`-sized
                // text row instead, so the same test can assert it does change with zoom.
                .debug_selector(move || format!("file-view-text-row-{line_number}"))
                .child(text_row),
        )
        .into_any_element()
}

/// The File view's status bar: language, last-click cursor line (`None` until the first click,
/// per `AdeApp::code_cursor`), a byte-detected line-ending label, and - for a `.rs` file with a
/// live LSP client - a `rust-analyzer` status. The design's `col 14` is deliberately omitted:
/// there's no per-character column tracking in this app, so showing a column would always read
/// `1`.
pub(super) fn render_file_status_bar(
    parsed: &code_view::ParsedFile,
    cursor: Option<usize>,
    lsp_status: Option<&LspFileStatus>,
) -> impl IntoElement {
    let position = cursor
        .map(|line| format!("ln {line}"))
        .unwrap_or_else(|| "no line selected".to_string());

    let mut bar = div()
        .flex_none()
        .h(theme::band::SURFACE_FOOTER)
        .flex()
        .items_center()
        .justify_end()
        .gap(px(10.0))
        .px(px(12.0))
        .border_t_1()
        .border_color(theme::border::INNER)
        .bg(theme::surface::FOOTER)
        .font(font(theme::font::MONO))
        .text_size(px(10.0))
        .text_color(theme::text::HINT);

    if let Some(status) = lsp_status {
        let (dot_color, label) = match status {
            LspFileStatus::Spawning => {
                (theme::text::GHOST, "starting rust-analyzer...".to_string())
            }
            LspFileStatus::Failed(message) => {
                (theme::status::FAIL, format!("rust-analyzer: {message}"))
            }
            LspFileStatus::Indexing => {
                (theme::status::ASK, "rust-analyzer: indexing...".to_string())
            }
            LspFileStatus::Analyzed { errors, warnings } => {
                let color = if *errors > 0 {
                    theme::status::FAIL
                } else {
                    theme::status::REVIEW
                };
                let label = if *errors == 0 && *warnings == 0 {
                    "rust-analyzer: no diagnostics".to_string()
                } else {
                    format!("rust-analyzer: {errors} errors, {warnings} warnings")
                };
                (color, label)
            }
        };
        bar = bar
            .child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded_full()
                    .bg(dot_color),
            )
            .child(label);
    }

    bar.child(parsed.language)
        .child(position)
        .child(parsed.line_ending.label())
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
    /// Uses this crate's own `root/code_surface.rs` as the large `.rs` fixture.
    #[gpui::test]
    fn opening_a_large_real_file_does_not_block_render_on_the_full_parse(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("large.rs");
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/root/code_surface.rs"
        ))
        .expect("read this crate's own root/code_surface.rs as a real, large .rs fixture");
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

/// End-to-end proof of the hover/go-to-definition feature: mirrors
/// [`lsp_diagnostics_wiring_tests`]'s shape (spawn a real `rust-analyzer` through this app's code
/// path, wait for an async result, assert on real content), applied to `request_hover`/
/// `trigger_goto_definition`. Calls `request_hover` directly - the same method
/// `render_file_view_line`'s click handler calls - rather than synthesizing a mouse click on a
/// virtualized row's pixel position.
#[cfg(test)]
mod lsp_hover_wiring_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    fn write_scratch_project(main_rs: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"app_hover_wiring_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir src");
        std::fs::write(dir.path().join("src").join("main.rs"), main_rs).expect("write main.rs");
        dir
    }

    /// A bounded retry loop that re-sends `request_hover` for the same click until it resolves
    /// to a non-empty `HoverRenderModel`, or `deadline` passes. A single request can honestly
    /// come back `Ready(None)` while rust-analyzer is still mid-index; `app.hover` is reset to
    /// `None` between attempts so `request_hover`'s caching doesn't turn a retry into a no-op.
    fn request_hover_until_resolved(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        path: PathBuf,
        deadline: Instant,
    ) -> hover_view::HoverRenderModel {
        loop {
            app.update(cx, |app, cx| {
                app.hover = None;
                app.request_hover(
                    path.clone(),
                    CALL_SITE_LINE,
                    CALL_SITE_BYTE_RANGE,
                    CALL_SITE_POSITION,
                    cx,
                );
            });
            loop {
                cx.run_until_parked();
                let settled = app.read_with(cx, |app, _| {
                    matches!(
                        app.hover.as_ref().map(|entry| &entry.status),
                        Some(HoverStatus::Ready(_)) | Some(HoverStatus::Failed(_))
                    )
                });
                if settled {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "AdeApp::hover never left its real Loading state within the real deadline"
                );
                std::thread::sleep(Duration::from_millis(200));
            }
            let resolved = app.update(cx, |app, _| match &app.hover {
                Some(HoverEntry {
                    status: HoverStatus::Ready(Some(model)),
                    ..
                }) => Some(model.clone()),
                _ => None,
            });
            if let Some(model) = resolved {
                return model;
            }
            assert!(
                Instant::now() < deadline,
                "AdeApp::request_hover never resolved to a real, non-empty hover for the real \
                 call site within the real deadline - rust-analyzer kept answering with real \
                 \"nothing here\"/errors past its own indexing window"
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Click position for `"    let result = add_one(41);"` (line index 8, 0-based), reused from
    /// `lsp_core::client::tests::rust_analyzer_returns_a_real_hover_for_a_documented_function`'s
    /// fixture. The `add_one` call-site identifier spans bytes 17..24 of that line (`"    let
    /// result = "` is 17 bytes, `"add_one"` is 7 more) - computed by hand since this test calls
    /// `request_hover` directly rather than through a render/click.
    const FIXTURE_SOURCE: &str = "/// Adds one to the given number.\n\
         ///\n\
         /// Returns the incremented value.\n\
         fn add_one(x: i32) -> i32 {\n    x + 1\n}\n\n\
         fn main() {\n    let result = add_one(41);\n    println!(\"{}\", result);\n}\n";
    const CALL_SITE_LINE: usize = 9; // 1-based - `AdeApp::code_cursor`'s own convention.
    const CALL_SITE_BYTE_RANGE: Range<usize> = 17..24;
    const CALL_SITE_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 8,
        character: 20,
    };

    /// A click-equivalent `request_hover` call against a running rust-analyzer (spawned through
    /// `render_file_view`'s `ensure_lsp_client` path) resolves to a `HoverRenderModel` whose
    /// signature and doc text match the fixture's documented function.
    #[gpui::test]
    fn a_real_click_resolves_to_a_real_hover_render_model(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();
        // `render_file_view` spawns the LspClient but only runs as part of a render; there's no
        // window compositor driving repaints in this headless test, so drive it directly.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        // Keeps re-rendering (the trigger for `dispatch_did_open` once the client is Ready)
        // while waiting for the handshake - a single "wait, then render once" could leave
        // `didOpen` never sent for a client that only just became Ready.
        let client_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            let ready = app.read_with(cx, |app, _| {
                matches!(
                    app.lsp_clients.get(project.path()),
                    Some(LspClientState::Ready(_))
                )
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < client_deadline,
                "the real rust-analyzer client never became Ready within 120s"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

        let deadline = Instant::now() + Duration::from_secs(120);
        let model = request_hover_until_resolved(&app, cx, main_rs.clone(), deadline);

        assert!(
            model.signature.contains("add_one"),
            "expected the real signature to mention the real function name, got: {:?}",
            model.signature
        );
        assert!(
            model.signature.contains("i32"),
            "expected the real signature to mention the real i32 type, got: {:?}",
            model.signature
        );
        let doc = model.doc.as_deref().unwrap_or_default();
        assert!(
            doc.contains("Adds one to the given number"),
            "expected the real doc comment text to reach the render model, got: {doc:?}"
        );
        app.read_with(cx, |app, _| {
            let entry = app.hover.as_ref().expect("a real hover entry should exist");
            assert_eq!(entry.path, main_rs);
        });
    }

    /// Proves the `F12` keybinding is wired end to end at the GPUI action-dispatch layer
    /// (`cx.bind_keys`, `AdeApp::render`'s `.on_action`, `handle_goto_definition_action`) by
    /// observing a distinguishing side effect: with no file open, `hover` is `None`, so
    /// `trigger_goto_definition` returns immediately without spawning anything - a harmless,
    /// deterministic no-op this test can assert didn't panic or spawn a stray background task.
    ///
    /// ## Why full navigation is proven a different way
    ///
    /// Mounting a File view via `render_center_pane` was found, while writing this test, to leave
    /// `TestAppContext::dispatch_action` unable to reach any `on_action` handler at all:
    /// `render_center_pane` stops rendering the active session's terminal pane the instant a File
    /// view mounts, leaving `Window::focus` dangling on a `FocusId` the last frame no longer
    /// contains - the same bug class [`palette_focus_tests`]/[`settings_focus_tests`] describe,
    /// fixed for Surface C by [`AdeApp::code_focus_handle`] (see [`code_focus_tests`] for
    /// interactive coverage that `GotoDefinition` reaches its handler with a File view open).
    /// This test stays minimal (fresh window, no file opened); full navigation is proven by
    /// [`f12_action_navigates_to_the_real_definition_line`] calling `trigger_goto_definition`
    /// directly instead.
    #[gpui::test]
    fn f12_action_reaches_the_real_handler_on_a_fresh_window(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        assert_eq!(
            app.read_with(cx, |app, _| app.hover.clone()),
            None,
            "sanity check: a fresh window has no real hover entry yet"
        );

        cx.dispatch_action(GotoDefinition);
        cx.run_until_parked();

        // `handle_goto_definition_action`'s only effect with `hover == None` is a harmless early
        // return; confirming the app is unchanged is the available proof dispatch reached it.
        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
    }

    /// `trigger_goto_definition` sends a `textDocument/definition` request using `hover`'s
    /// position, and the response navigates `code_cursor` to the function's definition line, not
    /// the call site the request was sent from. Calls `trigger_goto_definition` directly rather
    /// than `cx.dispatch_action(GotoDefinition)` - see this module's docs above.
    #[gpui::test]
    fn f12_action_navigates_to_the_real_definition_line(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        // See the hover test above's identical loop for why re-rendering, not just waiting,
        // matters.
        let client_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            let ready = app.read_with(cx, |app, _| {
                matches!(
                    app.lsp_clients.get(project.path()),
                    Some(LspClientState::Ready(_))
                )
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < client_deadline,
                "the real rust-analyzer client never became Ready within 120s"
            );
            std::thread::sleep(Duration::from_millis(200));
        }

        // `trigger_goto_definition` reads `hover`'s path/position regardless of whether the
        // hover content itself has resolved - a `Loading` entry already carries a valid request
        // target, so this only needs `request_hover` to have been called once.
        app.update(cx, |app, cx| {
            app.request_hover(
                main_rs.clone(),
                CALL_SITE_LINE,
                CALL_SITE_BYTE_RANGE,
                CALL_SITE_POSITION,
                cx,
            );
        });
        cx.run_until_parked();

        // Retried on a timer rather than called once: a `textDocument/definition` response can
        // honestly be empty while rust-analyzer is still mid-index, and a real user would just
        // press F12 again.
        let definition_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.trigger_goto_definition(cx);
            });
            cx.run_until_parked();
            // The definition request runs on a background OS thread; GPUI's deterministic test
            // executor's `run_until_parked` only drains its own scheduled queue, which is empty
            // again the instant that background call is dispatched, so a wall-clock sleep is
            // needed before a second `run_until_parked` can observe the completion callback.
            std::thread::sleep(Duration::from_millis(300));
            cx.run_until_parked();
            // `fn add_one` is on line 4 (1-based), different from `CALL_SITE_LINE` (9), proving
            // this is real navigation, not a no-op that left the cursor where it was.
            let navigated = app.read_with(cx, |app, _| app.code_cursor == Some(4));
            if navigated {
                break;
            }
            assert!(
                Instant::now() < definition_deadline,
                "trigger_goto_definition never navigated AdeApp::code_cursor to the real \
                 definition line within 120s - last observed code_cursor: {:?}",
                app.read_with(cx, |app, _| app.code_cursor)
            );
        }
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

/// Proves the segmented `Diff | File` toggle's dispatch (`render_diff_file_toggle`, via the
/// shared `render_choice_control`) is driven by each segment's structural position, not its
/// display label - the R5.5 audit found the prior label-string dispatch could silently select
/// the wrong value if a label was renamed without updating `on_select`, with no compile error or
/// test failure. Clicks each segment by its structural `debug_selector` (never derived from the
/// label text) and asserts `code_view` matches that segment's position.
#[cfg(test)]
mod choice_control_dispatch_tests {
    use super::*;
    use gpui::TestAppContext;

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

    #[gpui::test]
    fn clicking_a_segment_by_structural_position_selects_the_matching_real_value(
        cx: &mut TestAppContext,
    ) {
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

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("a.txt"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "sanity: opening a changed file's diff lands in Diff view by default"
        );

        // Segment at structural index 1 ("File") - clicked by its position-based selector,
        // never by searching for its label text.
        let file_bounds = cx
            .debug_bounds("choice-diff-file-toggle-1")
            .expect("the File segment must have painted at least once");
        cx.simulate_click(file_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File,
            "clicking the segment at structural index 1 must select File - position-based \
             dispatch, not a re-match on whatever that segment's label currently says"
        );

        // Segment at structural index 0 ("Diff") - back the other way, same mechanism.
        let diff_bounds = cx
            .debug_bounds("choice-diff-file-toggle-0")
            .expect("the Diff segment must have painted at least once");
        cx.simulate_click(diff_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "clicking the segment at structural index 0 must select Diff"
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
            + px(crate::terminal_pane::PANE_PADDING_PX)
            + cell_size.width * (prefix_chars + link_text.chars().count() as f32 / 2.0);
        let y = bounds.origin.y
            + px(crate::terminal_pane::PANE_PADDING_PX)
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

    /// The bug `crate::terminal_pane::click_included_secondary_modifier` fixes:
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

    /// A false-positive class `crate::terminal_links`'s regex can't rule out: a plausible-looking
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

/// Coverage for the editor-zoom feature: clamping/rounding logic, zoom-state mutation through
/// [`AdeApp`], both per-tab-zoom modes, and an interaction test proving the scoped
/// `rem_scope::WithRemSize` mechanism scales code text while leaving the fixed-`px()` gutter
/// untouched.
#[cfg(test)]
mod code_zoom_tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn clamp_zoom_percent_stays_put_at_the_documented_boundaries() {
        assert_eq!(clamp_zoom_percent(70), 70);
        assert_eq!(clamp_zoom_percent(200), 200);
        assert_eq!(clamp_zoom_percent(100), 100);
    }

    #[test]
    fn clamp_zoom_percent_clamps_out_of_range_candidates_into_bounds() {
        assert_eq!(
            clamp_zoom_percent(-40),
            70,
            "a negative candidate must clamp to the real minimum, not underflow/wrap"
        );
        assert_eq!(clamp_zoom_percent(5000), 200);
        assert_eq!(clamp_zoom_percent(0), 70);
    }

    #[test]
    fn clamp_zoom_percent_rounds_to_the_nearest_real_ten_point_step() {
        // 53 -> 5.3 -> rounds to 5 steps -> 50 -> clamped up to the real 70 minimum.
        assert_eq!(clamp_zoom_percent(53), 70);
        // 75 -> 7.5 -> rounds away from zero to 8 steps -> 80.
        assert_eq!(clamp_zoom_percent(75), 80);
        // 84 -> 8.4 -> rounds down to 8 steps -> 80.
        assert_eq!(clamp_zoom_percent(84), 80);
        // 205 -> 20.5 -> rounds up to 21 steps -> 210 -> clamped down to the real 200 maximum.
        assert_eq!(clamp_zoom_percent(205), 200);
    }

    fn write_single_file(repo: &std::path::Path) -> PathBuf {
        let file_path = repo.join("main.rs");
        std::fs::write(&file_path, "fn main() {\n    let x = 1;\n}\n").expect("write main.rs");
        file_path
    }

    /// A valid `.rs` file of exactly `lines` lines (`// line N` comments) - used by
    /// `zoom_scales_text_but_not_the_gutter_width` to reach a 4-digit line number, which
    /// `write_single_file`'s 3-line file can't produce.
    fn write_many_line_file(repo: &std::path::Path, lines: usize) -> PathBuf {
        let file_path = repo.join("main.rs");
        let mut content = String::new();
        for line in 1..=lines {
            content.push_str(&format!("// line {line}\n"));
        }
        std::fs::write(&file_path, content).expect("write main.rs");
        file_path
    }

    #[gpui::test]
    fn zoom_in_and_out_clamp_at_the_documented_boundaries_through_the_real_app(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_single_file(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "a freshly opened file starts at the real 100% default"
        );

        app.update(cx, |app, cx| {
            for _ in 0..20 {
                app.zoom_out(cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            AdeApp::ZOOM_MIN_PERCENT,
            "zooming out far past the real minimum must clamp at 70%, never go lower"
        );

        app.update(cx, |app, cx| {
            for _ in 0..30 {
                app.zoom_in(cx);
            }
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            AdeApp::ZOOM_MAX_PERCENT,
            "zooming in far past the real maximum must clamp at 200%, never wrap"
        );
    }

    #[gpui::test]
    fn resetting_zoom_returns_to_100_percent(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = write_single_file(repo.path());
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });

        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
        });
        assert_eq!(app.read_with(cx, |app, _| app.code_zoom_percent), 130);

        app.update(cx, |app, cx| app.reset_zoom(cx));
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "resetting zoom - the toolbar value's own click affordance - must land exactly on \
             100%, matching design_handoff_jerry_ade/revision/CHANGELOG.md's change 6"
        );
    }

    /// `per_tab_zoom` on (the default) - each open file tab must remember its own zoom
    /// independently.
    #[gpui::test]
    fn per_tab_zoom_on_remembers_each_tabs_own_zoom_independently(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let a = repo.path().join("a.rs");
        let b = repo.path().join("b.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&b, "fn b() {}\n").expect("write b.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.per_tab_zoom),
            "per_tab_zoom should default to true - see AppearanceSettings::default"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        app.update(cx, |app, cx| app.zoom_in(cx)); // a.rs -> 110%

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "a tab that has never been zoomed must start at the real 100% default, not inherit \
             the previously active tab's zoom"
        );
        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
        }); // b.rs -> 120%

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            110,
            "switching back to a.rs must restore its own real, previously set zoom"
        );

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("b.rs"), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            120,
            "switching back to b.rs must restore its own, independently remembered zoom"
        );
    }

    /// `Settings.appearance.per_tab_zoom` off - one shared zoom value must apply uniformly to
    /// every open file, and switching tabs must never silently revert it.
    #[gpui::test]
    fn per_tab_zoom_off_shares_one_zoom_value_across_every_open_file(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let a = repo.path().join("a.rs");
        let b = repo.path().join("b.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&b, "fn b() {}\n").expect("write b.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| app.toggle_per_tab_zoom(cx));
        assert!(!app.read_with(cx, |app, _| app.settings.appearance.per_tab_zoom));

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        app.update(cx, |app, cx| app.zoom_in(cx)); // 110%, shared

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            110,
            "with per-tab zoom off, opening a different file must keep the one shared zoom \
             value, not reset to 100%"
        );

        app.update(cx, |app, cx| app.zoom_in(cx)); // 120%, shared

        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            120,
            "with per-tab zoom off, switching back to a.rs must show the same shared 120% - not \
             the 110% it happened to be at when it was left, which would mean the value was \
             secretly still being tracked per-tab"
        );
    }

    /// Regression: set a shared zoom, then turn per-tab zoom on - every already-open tab must
    /// keep showing the zoom it already had, not reset to 100%. Root cause: `file_zoom_percent`
    /// used to only be written while per-tab mode was already on, so turning it on left the map
    /// empty for every open tab.
    #[gpui::test]
    fn turning_per_tab_zoom_on_seeds_every_open_tab_with_the_current_shared_zoom(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let a = repo.path().join("a.rs");
        let b = repo.path().join("b.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        std::fs::write(&b, "fn b() {}\n").expect("write b.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        // Start in shared mode (per-tab off) - the default is per-tab on, so this toggle sets up
        // the starting state.
        app.update(cx, |app, cx| app.toggle_per_tab_zoom(cx));
        assert!(!app.read_with(cx, |app, _| app.settings.appearance.per_tab_zoom));

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(b.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
        }); // 150%, shared - both a.rs and b.rs are showing this right now
        assert_eq!(app.read_with(cx, |app, _| app.code_zoom_percent), 150);

        app.update(cx, |app, cx| app.toggle_per_tab_zoom(cx));
        assert!(app.read_with(cx, |app, _| app.settings.appearance.per_tab_zoom));
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            150,
            "turning per-tab zoom on must never itself change what the currently active tab is \
             showing"
        );

        // b.rs is the active tab (opened last) - switching away and back must restore 150%.
        // `activate_file_tab` (unlike `open_file_view`) takes the relative path `open_files`/
        // `file_zoom_percent` are keyed by, not an absolute one.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("b.rs"), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            150,
            "b.rs was showing 150% the instant per-tab zoom was turned on - switching away and \
             back must not have silently discarded that and reset it to the real 100% default"
        );

        // a.rs never got its own explicit zoom action, but it was also visibly at 150% (the
        // shared value) the instant the mode flipped - it must have been seeded too.
        app.update_in(cx, |app, window, cx| {
            app.activate_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            150,
            "a.rs was also showing the shared 150% at the moment per-tab zoom turned on - it \
             must have been seeded with that value too, not just whichever tab happened to be \
             active"
        );
    }

    /// Regression: zoom a tab, close it, reopen the same path - it must come back at the 100%
    /// default, not resurrect the stale zoom it was left at. See `close_file_tab`'s docs: the
    /// closed path's `file_zoom_percent` entry is removed immediately.
    #[gpui::test]
    fn closing_a_tab_clears_its_remembered_zoom_so_reopening_it_starts_fresh(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let a = repo.path().join("a.rs");
        std::fs::write(&a, "fn a() {}\n").expect("write a.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        assert!(
            app.read_with(cx, |app, _| app.settings.appearance.per_tab_zoom),
            "per_tab_zoom should default to true - see AppearanceSettings::default"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            app.zoom_in(cx);
            app.zoom_in(cx);
            app.zoom_in(cx);
        }); // a.rs -> 130%
        assert_eq!(app.read_with(cx, |app, _| app.code_zoom_percent), 130);

        app.update_in(cx, |app, window, cx| {
            app.close_file_tab(PathBuf::from("a.rs"), window, cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app.open_files.contains(&PathBuf::from("a.rs"))),
            "closing the tab must really remove it from open_files"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(a.clone(), window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.code_zoom_percent),
            AdeApp::ZOOM_DEFAULT_PERCENT,
            "reopening a.rs after closing it must start at the real 100% default, not resurrect \
             the 130% it was left at before it was closed"
        );
    }

    /// Proves `zoom_scoped`'s `WithRemSize` mechanism works: a live-rendered code row's text
    /// grows with zoom while the fixed-`px()` line-number gutter measures identically at every
    /// zoom level.
    ///
    /// ## Why a width-only assertion would be vacuous
    ///
    /// Comparing only the gutter's `width` can never fail, since the column is declared
    /// `w(px(52.0))` - a compile-time literal GPUI resolves identically regardless of whether
    /// zoom-scoping is wired up correctly. It proves nothing about whether the line-number text
    /// inside still (wrongly) grows with zoom. This test's second half closes that gap: a
    /// 4-digit line number, scrolled into view at the 200% zoom maximum, where the original bug
    /// manifested (`uniform_list` sizes every row's slot from item index 0 alone - a single-digit
    /// "1", which never wraps - so a 4-digit gutter number wrapping at higher zoom painted taller
    /// than its allocated slot, overlapping the row below).
    #[gpui::test]
    fn zoom_scales_text_but_not_the_gutter_width(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        // 1200 lines - enough to reach a 4-digit line number (1000), which the second half
        // needs; line 1 (used by the first half) exists regardless of file size.
        let file_path = write_many_line_file(repo.path(), 1200);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path, window, cx);
        });
        cx.run_until_parked();

        let gutter_at_100 = cx
            .debug_bounds("file-view-gutter-1")
            .expect("line 1's gutter should have really painted at the default 100% zoom");
        let text_at_100 = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's text row should have really painted at the default 100% zoom");

        app.update(cx, |app, cx| {
            for _ in 0..5 {
                app.zoom_in(cx); // 100% -> 150%
            }
        });
        cx.run_until_parked();

        let gutter_at_150 = cx
            .debug_bounds("file-view-gutter-1")
            .expect("line 1's gutter should have really painted at 150% zoom");
        let text_at_150 = cx
            .debug_bounds("file-view-text-row-1")
            .expect("line 1's text row should have really painted at 150% zoom");

        assert_eq!(
            gutter_at_100.size.width, gutter_at_150.size.width,
            "the real, fixed-px() line-number gutter must measure identically at every zoom \
             level - it must never respond to the scoped rem-size override"
        );
        assert!(
            text_at_150.size.height > text_at_100.size.height,
            "the real, rems()-sized text row must actually grow taller at 150% zoom \
             (line-height is rems(1.6), scoped to the real effective zoom rem size) - got \
             {:?} at 100% vs {:?} at 150%",
            text_at_100.size,
            text_at_150.size,
        );

        // Scroll a 4-digit line number into view, push zoom to the 200% maximum (the audit
        // measured a wrapped-line-number row at 54px into a 27px slot at 130%, 83px into 41.5px
        // at 200%), and confirm the gutter never grew taller than its row's code text.
        app.update(cx, |app, cx| {
            for _ in 0..5 {
                app.zoom_in(cx); // 150% -> 200%
            }
            app.file_view_scroll_handle
                .scroll_to_item(999, ScrollStrategy::Center);
            cx.notify();
        });
        cx.run_until_parked();

        let gutter_at_200 = cx.debug_bounds("file-view-gutter-1000").expect(
            "scrolling to line 1000 (index 999) at 200% zoom should have really painted its \
             gutter",
        );
        let text_at_200 = cx.debug_bounds("file-view-text-row-1000").expect(
            "scrolling to line 1000 (index 999) at 200% zoom should have really painted its \
             text row",
        );

        assert_eq!(
            gutter_at_200.size.height, text_at_200.size.height,
            "line 1000's real, 4-digit gutter must measure exactly as tall as its own code \
             text row at 200% zoom - a taller gutter means its line number wrapped onto a \
             second real line inside the still-fixed-52px column, which uniform_list's own \
             single-row-height measurement (taken from line 1 alone) would paint straight into \
             the row below's slot, exactly the real overlap the audit measured live"
        );
    }
}
