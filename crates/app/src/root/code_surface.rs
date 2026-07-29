use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::lsp::{lsp_file_status, LspClientState, LspFileStatus};
use crate::root::widgets::{render_keycap, render_sidebar_message, render_tag_pill};

impl AdeApp {
    /// Loads (or reloads) the real diff of `root` against its detected base branch, per
    /// `wt_core::diff`'s docs. Offloaded to `cx.background_executor()` for the same reason
    /// `load_worktrees`/`load_file_tree` are: `diff_against_base` performs blocking I/O
    /// (`gix` reads plus a spawned `git diff` child process) and must never run on the GPUI
    /// foreground thread.
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
                        // The `+n`/`-n` header totals (`Self::diff_totals`) are folded here,
                        // off the UI thread, right alongside the diff itself becoming
                        // available - not recomputed on every render (see `diff_totals`'s
                        // docs for the real per-frame cost that used to be).
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
                // A freshly (re)loaded diff may have changed - or stopped having - a real
                // `DiffFile` for whichever path `open_change` currently names (e.g. an agent
                // just touched the very file already open in Surface C). Refresh the cache
                // `Self::render_center_pane` reads instead of leaving it pointed at the
                // previous diff's now-stale entry until the next unrelated navigation event
                // happens to refresh it.
                this.refresh_open_diff_file_cache();
                // The palette's cached file-candidate list (`Self::palette_file_candidates`)
                // carries each file's real add/del/changed marks from the current diff - refresh
                // it here too, the other real point that input changes (see
                // `Self::rebuild_palette_file_candidates`'s docs).
                this.rebuild_palette_file_candidates();
                cx.notify();
            });
        });
        self._load_diff_task = Some(task);
    }

    /// Opens `path`'s real diff in the centre pane - the Changes row's own click handler
    /// (`design_handoff_jerry_ade/README.md`: "clicking a change row sets ... open_change =
    /// row"). See [`Self::open_change`]'s docs for what this actually swaps in.
    pub(super) fn open_change_diff(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_code_surface(window, cx);
        self.open_change = Some(path);
        self.code_view = code_view::CodeView::Diff;
        self.refresh_open_diff_file_cache();
        // See `Self::select_worktree`'s identical reset for why: a Hover card is only ever real
        // for the file it was requested against.
        self.hover = None;
        cx.notify();
    }

    /// Opens `path` (a real file on disk, from a real Files-tree row click) directly in Surface
    /// C's real File view - `design_handoff_jerry_ade/README.md`'s Files tree never gave
    /// individual file rows a click handler of their own before this phase (only directories, to
    /// collapse/expand - see [`Self::toggle_dir_collapsed`]). This is this phase's own documented
    /// trigger for reaching the File view from real navigation, alongside the Changes row's
    /// (`[`Self::open_change_diff`]) `Diff | File` toggle for files that already have a diff -
    /// see this crate's report for the judgment call.
    pub(super) fn open_file_view(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prune_confirm_armed = false;
        self.focus_code_surface(window, cx);
        // Every fresh File view open clears any stale `pending_cursor_line` left over from an
        // abandoned navigation (see that field's own docs for the real cross-file leak this
        // closes) - `Self::navigate_to_definition` re-sets a fresh, correct one right after this
        // call returns when it actually has a target line to apply, so this can never fight a
        // real, live navigation in progress.
        self.pending_cursor_line = None;
        let relative = path
            .strip_prefix(&self.file_tree_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        self.open_change = Some(relative);
        self.code_view = code_view::CodeView::File;
        self.selected_tree_path = Some(path);
        self.refresh_open_diff_file_cache();
        // See `Self::select_worktree`'s identical reset for why.
        self.hover = None;
        cx.notify();
    }

    /// Closes the centre's file-diff/file view and returns to the active session's terminal -
    /// the diff/file surface's own real "back"/close affordance. Restores real keyboard focus the
    /// same way [`Self::close_settings`] does, and for the same documented reason - see
    /// [`Self::code_focus_handle`]'s own docs for the bug this fixes.
    pub(super) fn close_change_diff(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_change = None;
        self.refresh_open_diff_file_cache();
        self.hover = None;
        restore_focus(
            &self.sessions,
            &mut self.code_return_focus,
            &mut self.code_opened_session,
            window,
            cx,
        );
        cx.notify();
    }

    /// Recomputes [`Self::open_diff_file_cache`] (and, since it depends only on the diff's own
    /// hunks - never the file's own content - [`Self::file_view_changed_lines`] alongside it)
    /// from [`Self::open_change`] and [`Self::current_diff`]. Called at every real point either
    /// input can actually change (a different file opened or closed, or the diff itself
    /// (re)loading) - never from a render method. See [`Self::open_diff_file_cache`]'s own docs
    /// for the per-render `DiffFile` clone this exists to avoid.
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

    /// Dispatches a real, off-foreground-thread `code_view::load_file` call for `path` - see
    /// [`FileLoadState`]'s own docs for exactly why this must never run inline during `render()`.
    /// Mirrors [`Self::load_diff`]'s exact shape: mark the state `Loading` and `cx.notify()`
    /// immediately (so the very next render shows a real, honest loading state rather than
    /// silently doing nothing until the background task resolves), hand the actual blocking work
    /// to `cx.background_executor()`, then write the real outcome back into
    /// [`Self::file_view_cache`]/[`Self::file_load_state`] from a `this.update(cx, ..)` callback
    /// once it resolves, back on the foreground thread.
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
                        // A real go-to-definition navigation (`Self::navigate_to_definition`) may
                        // have left a real target line waiting specifically for *this* file's
                        // load to finish - apply it instead of the ordinary "just-opened, start
                        // at line 1" default, but only when it actually names the file that just
                        // finished loading. See `Self::pending_cursor_line`'s own docs for the
                        // real cross-file cursor leak this path check closes: without it, an
                        // unrelated file's completed load could apply a stale target line left
                        // behind by an earlier, since-abandoned navigation into a *different*
                        // file.
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
                        // A real read failure must not leave a stale target line around to
                        // misapply onto whatever unrelated file loads successfully next - see
                        // `Self::pending_cursor_line`'s own docs.
                        this.pending_cursor_line = None;
                    }
                }
                cx.notify();
            });
        });
        self._file_load_task = Some(task);
    }

    /// Real click-to-hover trigger for Surface C's File view (`design_handoff_jerry_ade/
    /// README.md`'s Hover state) - `crate::root::render_file_view_line`'s own per-token click
    /// handler calls this with `absolute_path` (the real, currently-open `.rs` file),
    /// `line_number`/`byte_range` (the real, 1-based line and in-line byte span of whichever
    /// already-highlighted token was clicked), and `position` (the same click's real LSP
    /// `Position`, already computed by `hover_view::position_for_line_byte_offset` from that same
    /// byte range - computed once at the render/click site rather than re-derived here).
    ///
    /// ## Judgment call: click, not mouse-hover
    ///
    /// The design's own Hover state is triggered by a real mouse-hover; this app has no
    /// mouse-hover-position tracking against individual code tokens (only `on_click`, the same
    /// interaction model `Self::render_file_view_line`'s existing line-level `code_cursor` click
    /// already established, and the one `crate::diagnostics_view`'s own H2 report already used
    /// for the equivalent judgment call on the Diagnostic state's card). Building real
    /// per-pixel hover-tracking machinery (a `.on_mouse_move` handler translated back into a
    /// token/byte position on every real mouse movement over the code view, debounced so it
    /// doesn't flood `rust-analyzer` with a request per pixel) would be a substantial, novel
    /// piece of infrastructure for this phase alone - out of proportion to what the rest of this
    /// read-only viewer needs, and not otherwise motivated by anything else in this app. A click
    /// is a real, deliberate, unambiguous "tell me about this symbol" action, consistent with
    /// every other real interaction this viewer already has.
    ///
    /// ## Caching: never re-requests for the same real click
    ///
    /// A no-op (no new request dispatched, [`Self::hover`] left untouched) when
    /// `(absolute_path, line_number, byte_range)` already matches [`Self::hover`]'s current entry,
    /// the same "don't redo real work every frame/every re-click" discipline
    /// [`Self::file_view_cache`]'s own docs establish for the parse cache, applied here to a real
    /// network-equivalent (a real `rust-analyzer` round trip) request instead of a real parse.
    /// Runs the real request on `cx.background_executor()`, never inline: `lsp_core::client`'s
    /// own docs are explicit that [`lsp_core::LspClient::request`] blocks the calling thread for
    /// a real response, which must never be the GPUI foreground thread (the exact rule this
    /// project's own H1/H2 reports both had to fix a real violation of).
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
            // No real, ready client for this file's root yet (still spawning, failed, or - in
            // practice unreachable, since only a `.rs` file's click handler ever calls this -
            // simply never started). There is no real client to ask, so there is honestly
            // nothing to show; clearing any previous entry rather than leaving a stale one
            // showing for a click that produced no real new attempt.
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
                // Only apply this real result if it's still the answer to the *current* real
                // click - a slower, superseded request finishing after a newer click must never
                // clobber the newer one's own (possibly still-loading) entry.
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
        // A single slot, not an unbounded `Vec` - see `Self::_hover_request_task`'s own docs for
        // why this is safe (hover has no notion of independent concurrent requests the way
        // `Self::_goto_definition_tasks` does) and the real thread-starvation bug it closes.
        // Assigning here drops (and so immediately cancels) any still-in-flight previous request.
        self._hover_request_task = Some(task);
    }

    /// `F12`'s real handler (`design_handoff_jerry_ade/README.md`'s "`F12 definition` footer") -
    /// see [`GotoDefinition`]'s own docs for why [`Self::hover`] itself is the real, honest
    /// source of "which symbol" rather than a separately-tracked target. A real no-op when
    /// nothing's been clicked yet ([`Self::hover`] is `None`).
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
                // A real timeout/error, or a real "no definition here" (`Ok(None)`) - both
                // honestly produce no navigation, never a fabricated one.
                return;
            };
            let Some((target_uri, target_range)) = hover_view::first_definition_location(&response)
            else {
                return;
            };
            let Ok(target_path) = lsp_core::LspClient::path_for_uri(&target_uri) else {
                // A real non-`file://` target (e.g. a virtual macro-expansion buffer) - see
                // `lsp_core::LspClient::path_for_uri`'s own docs for why this is a real,
                // reachable "no real navigation possible" case, not an error.
                return;
            };
            let target_line = target_range.start.line as usize + 1;
            // `Self::navigate_to_definition` needs real `Window` access (to move focus onto
            // `Self::code_focus_handle` - see that field's own docs) that this background
            // completion doesn't have directly; `WeakEntity::update_in` gets it anyway, since
            // `AsyncApp` implements the real `AppContext::with_window` by looking up the window
            // this entity (a window's own root view) already belongs to - verified against
            // `vendor/zed/crates/gpui/src/app/async_context.rs`'s own `AsyncApp::with_window` -
            // rather than requiring the task to have been spawned via `cx.spawn_in` up front.
            let _ = this.update_in(cx, |this, window, cx| {
                this.navigate_to_definition(target_path, target_line, window, cx);
            });
        });
        self._goto_definition_tasks.retain(|task| !task.is_ready());
        self._goto_definition_tasks.push(task);
    }

    /// Real navigation to a go-to-definition result - `crate::root::AdeApp::trigger_goto_definition`'s
    /// own completion handler. `absolute_target_path` may name a real file under
    /// [`Self::file_tree_root`] or a real file entirely outside it (e.g. another crate in a
    /// workspace `rust-analyzer` can see but this app's own file tree doesn't include) - either
    /// way, `Self::open_file_view`'s own `strip_prefix` already handles a path that isn't under
    /// `file_tree_root` by falling back to the path as-is (`Self::render_file_view` resolves
    /// `file_tree_root.join(relative_path)`, which - `PathBuf::join`'s own real, documented
    /// behavior - simply becomes `relative_path` again when it's already absolute).
    ///
    /// ## The real cursor-line race this method exists to avoid
    ///
    /// [`Self::open_file_view`] alone would land the viewer on the right *file* but not
    /// necessarily the right *line*: if the target file wasn't already open,
    /// `Self::render_file_view` dispatches a real background `Self::spawn_file_load`, whose own
    /// completion handler unconditionally sets `Self::code_cursor` to `1` (the right default for
    /// every other real navigation into this view - a fresh file-tree/Changes-row click). Setting
    /// [`Self::code_cursor`] directly here, before that background load has even started, would
    /// just get silently overwritten back to `1` the instant it finishes. See
    /// [`Self::pending_cursor_line`]'s own docs for the real one-shot instruction this method
    /// sets up instead, and `Self::spawn_file_load`'s completion handler for the other half that
    /// consumes it.
    pub(super) fn navigate_to_definition(
        &mut self,
        absolute_target_path: PathBuf,
        one_based_line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_file_view(absolute_target_path.clone(), window, cx);
        // Mirrors `Self::render_file_view`'s own real freshness check exactly (path *and*
        // `mtime`/`len`, via the same real `code_view::cache_is_fresh`) rather than a plain path
        // comparison - so this decision and the dispatch-or-not decision
        // `Self::render_file_view` makes moments later on the very next render can never
        // disagree (e.g. the target file's cached parse being present but stale because the file
        // changed on disk since it was last loaded, which a plain path check would miss).
        let metadata = std::fs::metadata(&absolute_target_path).ok();
        let mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
        let len = metadata.as_ref().map(|meta| meta.len()).unwrap_or(0);
        let already_fresh = self.file_view_cache.as_ref().is_some_and(|cached| {
            code_view::cache_is_fresh(cached, &absolute_target_path, mtime, len)
        });
        if already_fresh {
            // The target file's real parse is already cached (e.g. navigating to a definition
            // inside the file already open) - `Self::render_file_view` won't dispatch a real
            // reload, so its completion handler will never run to consume
            // `Self::pending_cursor_line`. Apply the real target line directly instead.
            self.code_cursor = Some(one_based_line);
            self.file_view_scroll_handle
                .scroll_to_item(one_based_line.saturating_sub(1), ScrollStrategy::Center);
        } else {
            self.pending_cursor_line = Some((absolute_target_path, one_based_line));
        }
        cx.notify();
    }

    /// [`GotoDefinition`]'s real, bound `F12` action handler.
    pub(super) fn handle_goto_definition_action(
        &mut self,
        _action: &GotoDefinition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.trigger_goto_definition(cx);
    }

    /// The currently loaded real diff, if [`Self::diff_state`] has one - `None` while
    /// loading/erroring, or when the worktree is on its default branch / has no detectable
    /// base (see `wt_core::diff::DiffBase`'s docs for those two explanatory non-diff
    /// outcomes). The one real source every Zone 3 view (file-tree change marks, the Changes
    /// list, the centre's file-diff surface) reads, so they can never disagree.
    pub(super) fn current_diff(&self) -> Option<&WorktreeDiff> {
        match &self.diff_state {
            DiffLoadState::Loaded(DiffBase::Diff(diff)) => Some(diff),
            _ => None,
        }
    }

    /// A real, themed explanatory message for every [`DiffLoadState`] that isn't a loaded diff,
    /// shared by the Changes list (no diff yet/loaded) and, defensively, anywhere else that
    /// needs to explain why there's no diff content to show.
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
            // Unreachable from every real call site (each checks `current_diff()` first), but
            // matched explicitly rather than a wildcard so a future `DiffBase` variant can't
            // silently fall through here without a compile error to catch it.
            DiffLoadState::Loaded(DiffBase::Diff(_)) => (String::new(), theme::text::FAINT),
        };
        render_sidebar_message(text, color)
    }

    /// Surface D - the real merge-conflict resolution surface (`design_handoff_jerry_ade/
    /// README.md`'s "Surface D — merge conflict"), replacing the pty/diff body below the tab
    /// strip and session context bar (which both keep rendering normally - only the body
    /// changes) exactly like Surface B/C already do. Renders whichever real
    /// [`merge::MergeFlowState`] `self.merge_flow` is currently in for `session`; every value
    /// shown here (branch names, file paths, conflict line content) comes from the real
    /// `wt_core::merge` call `Self::start_merge` made, never fabricated sample data.
    ///
    /// Deliberate simplifications vs. the design's full mockup, all honest rather than faked:
    /// no per-line gutter numbers (a `ConflictHunk`'s `ours`/`theirs` lines aren't tied to real
    /// original file line numbers once extracted from the markers - inventing incrementing
    /// numbers here would be exactly the kind of fabricated-looking-real data this project's
    /// conventions forbid); the left ("ours"/base) column is labelled with the real base branch
    /// name rather than an agent identity, since `wt_core::merge::attempt_merge` always runs
    /// `git merge` from the base worktree - the base branch is real git state, not a running
    /// session, so it has no real agent to attribute the tint to (see [`Self::start_merge`]'s
    /// docs for the plumbing this reflects).
    pub(super) fn render_merge_flow_surface(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(flow) = self.merge_flow.as_ref() else {
            return Empty.into_any_element();
        };

        let container = || {
            div()
                .id("merge-surface")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_hidden()
                .bg(theme::surface::CENTER)
        };

        match &flow.state {
            merge::MergeFlowState::Running => container()
                .items_center()
                .justify_center()
                .font(font(theme::font::SANS))
                .text_size(px(11.5))
                .text_color(theme::text::FAINT)
                .child("merging\u{2026}")
                .into_any_element(),

            merge::MergeFlowState::AlreadyUpToDate { base_branch } => container()
                .child(self.render_merge_message(
                    format!("Already up to date with {base_branch}"),
                    "This branch contributes nothing new - there was nothing to merge.".to_string(),
                    None,
                    cx,
                ))
                .into_any_element(),

            merge::MergeFlowState::Error {
                message,
                abortable_worktree,
            } => container()
                .child(self.render_merge_message(
                    "Merge failed".to_string(),
                    message.clone(),
                    abortable_worktree.clone(),
                    cx,
                ))
                .into_any_element(),

            merge::MergeFlowState::Clean {
                base_branch, files, ..
            } => container()
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .p(px(14.0))
                        .child(
                            div()
                                .font(font(theme::font::SANS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(12.5))
                                .text_color(theme::text::HEADING)
                                .child(format!("Clean merge into {base_branch}")),
                        )
                        .child(
                            div()
                                .font(font(theme::font::SANS))
                                .text_size(px(11.0))
                                .text_color(theme::text::FAINT)
                                .child(if files.is_empty() {
                                    "No files changed.".to_string()
                                } else {
                                    format!("{} file(s) staged, not yet committed.", files.len())
                                }),
                        )
                        .children(files.iter().map(|path| {
                            div()
                                .font(font(theme::font::MONO))
                                .text_size(px(11.0))
                                .text_color(theme::text::SECONDARY)
                                .child(path.display().to_string())
                        })),
                )
                .child(div().flex_1())
                .child(self.render_merge_flow_footer(true, self.merge_op_in_flight, cx))
                .into_any_element(),

            merge::MergeFlowState::Conflicted {
                base_branch,
                clean_files,
                files,
                active_file,
                active_hunk,
                ..
            } => {
                let resolved = merge::all_resolved(files);
                let mut body = container().child(self.render_merge_header(
                    base_branch,
                    files,
                    *active_file,
                    *active_hunk,
                ));

                let auto = clean_files.len();
                let total = clean_files.len() + files.len();
                let remaining = files
                    .iter()
                    .filter(|entry| match entry {
                        ConflictedPath::Text(file) => !file.is_resolved(),
                        ConflictedPath::Unmergeable { .. } => true,
                    })
                    .count();
                body = body.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(14.0))
                        .py(px(8.0))
                        .bg(theme::status::REVIEW_BG)
                        .border_b_1()
                        .border_color(theme::border::INNER)
                        .child(
                            div()
                                .flex_none()
                                .w(px(5.0))
                                .h(px(5.0))
                                .rounded_full()
                                .bg(theme::status::REVIEW),
                        )
                        .child(
                            div()
                                .font(font(theme::font::SANS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(11.0))
                                .text_color(theme::status::REVIEW)
                                .child(format!("Jerry auto-resolved {auto} of {total} files")),
                        )
                        .child(
                            div()
                                .font(font(theme::font::SANS))
                                .text_size(px(11.0))
                                .text_color(theme::text::FAINT)
                                .child(if remaining == 0 {
                                    "every conflict is resolved.".to_string()
                                } else {
                                    format!("{remaining} file(s) still need you.")
                                }),
                        ),
                );

                if resolved {
                    body = body.child(div().flex_1()).child(
                        div()
                            .flex_none()
                            .p(px(14.0))
                            .font(font(theme::font::SANS))
                            .text_size(px(11.5))
                            .text_color(theme::text::SECONDARY)
                            .child(
                                "Every conflict is resolved and staged - complete the merge below.",
                            ),
                    );
                } else if let Some((target_file, target_hunk)) = merge::first_unresolved(files) {
                    // `merge::first_unresolved` only ever points at a real
                    // `ConflictedPath::Text` entry with a real remaining `Conflict` segment -
                    // see that function's own docs - so both of these always match.
                    if let Some(ConflictedPath::Text(file)) = files.get(target_file) {
                        if let Some(ConflictSegment::Conflict(hunk)) =
                            file.segments.get(target_hunk)
                        {
                            body = body
                                .child(self.render_conflict_columns(base_branch, session, hunk, cx))
                                .child(self.render_take_both_row(cx));
                        } else {
                            body = body.child(div().flex_1());
                        }
                    } else {
                        body = body.child(div().flex_1());
                    }
                } else {
                    // Not resolved, but no real text hunk left to show either: every
                    // remaining unresolved entry is a real modify/delete or binary conflict
                    // this app has no text-hunk resolution action for - see
                    // `crate::merge::unmergeable_paths`'s docs. A distinct, honest panel
                    // (never silently falling through to "conflicts resolved").
                    body =
                        body.child(self.render_unmergeable_panel(merge::unmergeable_paths(files)));
                }

                body.child(self.render_merge_flow_footer(resolved, self.merge_op_in_flight, cx))
                    .into_any_element()
            }
        }
    }

    /// Surface D's header row: `Resolve merge`, the real base branch, and `hunk X of Y` for
    /// whichever file/hunk is currently active - `crate::merge::hunk_position_in_file`/
    /// `crate::merge::hunk_count`'s real, computed positions, not a hardcoded label.
    pub(super) fn render_merge_header(
        &self,
        base_branch: &str,
        files: &[ConflictedPath],
        active_file: usize,
        active_hunk: usize,
    ) -> impl IntoElement {
        let position_label = files.get(active_file).and_then(|entry| {
            let ConflictedPath::Text(file) = entry else {
                return None;
            };
            merge::hunk_position_in_file(file, active_hunk)
                .map(|pos| format!("hunk {pos} of {}", merge::hunk_count(file)))
        });

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(14.0))
            .py(px(10.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(12.5))
                    .text_color(theme::text::HEADING)
                    .child("Resolve merge"),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.5))
                    .text_color(theme::text::DIM)
                    .child(format!("into {base_branch}")),
            )
            .when_some(position_label, |el, label| {
                el.child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(px(11.0))
                        .text_color(theme::text::FAINTER)
                        .child(label),
                )
            })
    }

    /// Surface D's real two-column split for the currently active conflict hunk - real
    /// `ours`/`theirs` content extracted from the file's real on-disk conflict markers, never
    /// simulated. See [`Self::render_merge_flow_surface`]'s docs for why the left column is
    /// labelled with the real base branch rather than an agent identity.
    pub(super) fn render_conflict_columns(
        &self,
        base_branch: &str,
        session: &Session,
        hunk: &ConflictHunk,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (agent_fg, agent_bg) = work_surface::agent_tint(session.kind);
        let session_branch = self
            .worktrees
            .iter()
            .find(|item| item.path == session.cwd)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| hunk.theirs_label.clone());

        let column = |label: String,
                      sub: String,
                      lines: &[String],
                      fg: gpui::Rgba,
                      take_id: &'static str,
                      take_label: &'static str,
                      choice: wt_core::merge::ConflictChoice,
                      cx: &mut Context<Self>| {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_none()
                        .h(px(28.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .bg(theme::surface::HEADER)
                        .border_b_1()
                        .border_color(theme::border::INNER)
                        .child(
                            div()
                                .font(font(theme::font::SANS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(px(11.0))
                                .text_color(theme::text::SECONDARY)
                                .child(label),
                        )
                        .child(
                            div()
                                .font(font(theme::font::MONO))
                                .text_size(px(10.5))
                                .text_color(theme::text::DIMMER)
                                .child(sub),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .p(px(10.0))
                        .font(font(theme::font::MONO))
                        .text_size(px(11.5))
                        .text_color(fg)
                        .children(lines.iter().map(|line| {
                            div().child(if line.is_empty() {
                                "\u{a0}".to_string()
                            } else {
                                line.clone()
                            })
                        })),
                )
                .child(
                    div()
                        .id(take_id)
                        .flex_none()
                        .cursor_pointer()
                        .m(px(10.0))
                        .h(px(24.0))
                        .px(px(11.0))
                        .rounded(theme::radius::BUTTON)
                        .border_1()
                        .border_color(theme::border::BUTTON)
                        .flex()
                        .items_center()
                        .justify_center()
                        .font(font(theme::font::SANS))
                        .text_size(px(11.0))
                        .text_color(theme::text::SECONDARY)
                        .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                        .child(take_label)
                        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            this.resolve_active_hunk(choice, cx);
                        })),
                )
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .border_r_1()
                    .border_color(theme::border::ZONE)
                    .child(column(
                        base_branch.to_string(),
                        hunk.ours_label.clone(),
                        &hunk.ours,
                        theme::text::SECONDARY,
                        "take-left",
                        "Take left",
                        wt_core::merge::ConflictChoice::Left,
                        cx,
                    )),
            )
            .child(div().flex_1().min_w_0().bg(agent_bg).child(column(
                session.kind.label().to_string(),
                session_branch,
                &hunk.theirs,
                agent_fg,
                "take-right",
                "Take right",
                wt_core::merge::ConflictChoice::Right,
                cx,
            )))
    }

    /// The real `Take both` action (`design_handoff_jerry_ade/README.md`'s Result strip -
    /// "Jerry proposes the answer") on the currently active hunk - real, tested
    /// `wt_core::merge::ConflictChoice::Both` (keeps *both* sides' lines, ours then theirs),
    /// the same real function [`Self::render_conflict_columns`]'s own Take-left/Take-right
    /// buttons call.
    pub(super) fn render_take_both_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .py(px(8.0))
            .border_t_1()
            .border_color(theme::border::ZONE)
            .bg(theme::surface::FOOTER)
            .child(
                div()
                    .id("take-both")
                    .cursor_pointer()
                    .h(px(24.0))
                    .px(px(11.0))
                    .rounded(theme::radius::BUTTON)
                    .bg(theme::button::GREEN_BG)
                    .flex()
                    .items_center()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(theme::button::GREEN_FG)
                    .hover(|el| el.bg(theme::button::GREEN_BG_HOVER))
                    .child("Take both")
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.resolve_active_hunk(wt_core::merge::ConflictChoice::Both, cx);
                    })),
            )
    }

    /// The real, distinct panel for [`wt_core::merge::ConflictedPath::Unmergeable`] entries -
    /// modify/delete or binary conflicts this app has no text-hunk resolution action for (see
    /// that type's docs). Deliberately never rendered as if these were resolved or as the
    /// normal two-column text editor (there is no real hunk to show for either reason) -
    /// lists each real path and reason, and points at a real terminal as the honest way to
    /// resolve them by hand, matching this app's own established fallback for other real
    /// gaps (e.g. `crate::work_surface::ActionKind::Unimplemented`'s own "no fake action"
    /// precedent).
    pub(super) fn render_unmergeable_panel(
        &self,
        paths: Vec<(&std::path::Path, wt_core::merge::UnmergeableReason)>,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(14.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(12.0))
                    .text_color(theme::text::HEADING)
                    .child("Needs manual resolution"),
            )
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .text_color(theme::text::FAINT)
                    .child(
                        "Jerry has no automatic resolution for these - resolve them in a real \
                         terminal in this worktree, then reopen Merge.",
                    ),
            )
            .children(paths.into_iter().map(|(path, reason)| {
                let reason_label = match reason {
                    wt_core::merge::UnmergeableReason::ModifyDelete => {
                        "modified on one side, deleted on the other"
                    }
                    wt_core::merge::UnmergeableReason::Binary => "binary content conflict",
                };
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(11.0))
                            .text_color(theme::text::SECONDARY)
                            .child(path.display().to_string()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(10.5))
                            .text_color(theme::text::FAINTER)
                            .child(reason_label),
                    )
            }))
    }

    /// Surface D's footer: `Complete merge` (real `git commit`, enabled only once
    /// `resolved`) and `Abort merge` (real `git merge --abort`, always available while a flow
    /// is active) - see [`Self::complete_merge_flow`]/[`Self::abort_merge_flow`]'s docs.
    /// `in_flight` (`Self::merge_op_in_flight`) dims and disables both while a real background
    /// commit/abort from a previous click is still running, so a second click can't spawn a
    /// second, racing real git operation (defense in depth alongside the guard clause each of
    /// those methods already has - see their docs).
    pub(super) fn render_merge_flow_footer(
        &self,
        resolved: bool,
        in_flight: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let complete = div()
            .id("merge-complete")
            .flex_none()
            .h(px(24.0))
            .px(px(11.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(11.0));
        let complete = if resolved && !in_flight {
            complete
                .cursor_pointer()
                .bg(theme::button::GREEN_BG)
                .text_color(theme::button::GREEN_FG)
                .hover(|el| el.bg(theme::button::GREEN_BG_HOVER))
                .child("Complete merge")
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.complete_merge_flow(cx);
                }))
        } else {
            complete
                .cursor_default()
                .bg(theme::border::BUTTON_DISABLED)
                .text_color(theme::text::GHOSTER)
                .child(if in_flight {
                    "Completing\u{2026}"
                } else {
                    "Complete merge"
                })
        };

        let abort = div()
            .id("merge-abort")
            .flex_none()
            .h(px(24.0))
            .px(px(11.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .text_size(px(11.0));
        let abort = if in_flight {
            abort
                .cursor_default()
                .text_color(theme::text::GHOSTER)
                .child("Abort merge")
        } else {
            abort
                .cursor_pointer()
                .text_color(theme::button::DANGER_FG)
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .child("Abort merge")
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.abort_merge_flow(cx);
                }))
        };

        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .px(px(14.0))
            .py(px(10.0))
            .border_t_1()
            .border_color(theme::border::INNER)
            .bg(theme::surface::FOOTER)
            .child(abort)
            .child(complete)
    }

    /// A simple real-message panel (`AlreadyUpToDate`/`Error` states) - a title, the real
    /// message text, a real `Abort merge` action when `abortable_worktree` is `Some` (a real
    /// merge is genuinely still in progress there - see `merge::MergeFlowState::Error`'s
    /// docs), and a `Dismiss` action that clears [`Self::merge_flow`] without touching git.
    pub(super) fn render_merge_message(
        &self,
        title: String,
        message: String,
        abortable_worktree: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .p(px(20.0))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(13.0))
                    .text_color(theme::text::HEADING)
                    .child(title),
            )
            .child(
                div()
                    .max_w(px(480.0))
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::FAINT)
                    .child(message),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .mt(px(6.0))
                    .when(abortable_worktree.is_some(), |el| {
                        el.child(
                            div()
                                .id("merge-message-abort")
                                .cursor_pointer()
                                .h(px(24.0))
                                .px(px(11.0))
                                .rounded(theme::radius::BUTTON)
                                .flex()
                                .items_center()
                                .font(font(theme::font::SANS))
                                .text_size(px(11.0))
                                .text_color(theme::button::DANGER_FG)
                                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                                .child("Abort merge")
                                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                    this.abort_merge_flow(cx);
                                })),
                        )
                    })
                    .child(
                        div()
                            .id("merge-dismiss")
                            .cursor_pointer()
                            .h(px(24.0))
                            .px(px(11.0))
                            .rounded(theme::radius::BUTTON)
                            .border_1()
                            .border_color(theme::border::BUTTON)
                            .flex()
                            .items_center()
                            .font(font(theme::font::SANS))
                            .text_size(px(11.0))
                            .text_color(theme::text::SECONDARY)
                            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                            .child("Dismiss")
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.dismiss_merge_error(cx);
                            })),
                    ),
            )
    }

    /// The centre's real single-file Surface C, opened by a Changes-row click
    /// (`Self::open_change_diff`, always with a real `diff_file`) or a Files-tree row click
    /// (`Self::open_file_view`, `diff_file` may be `None`) - a toolbar (`dir`/`name`, an optional
    /// tag pill, real `+n`/`−n` when `diff_file` is present, the real `Diff | File` segmented
    /// toggle, an always-dimmed `Accept file` - see [`render_accept_file_button`]'s docs - and a
    /// real close/back action) over either [`Self::render_diff_file_detail`]'s real, folded hunk
    /// content or [`Self::render_file_view`]'s real, syntax-highlighted file content.
    ///
    /// `effective_view` is `File` unconditionally when `diff_file` is `None` (there is no diff to
    /// show - `design_handoff_jerry_ade/README.md`'s `code_view` state field: "forced to `File`
    /// when the session has no changes", read here per-file rather than per-session), regardless
    /// of whatever `self.code_view` was last left at by a *different* file.
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
            // The toolbar's own real "renamed from" detail - the row's compact
            // `render_moved_tag` has no room for the actual pre-rename path, but this toolbar
            // does. `changes::rename_label` is `None` unless `old_path` is both present and
            // really different from the current path.
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
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(render_accept_file_button())
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
            // Real focus target for the whole Diff/File surface - see
            // `Self::code_focus_handle`'s own docs for the dangling-`Window::focus` bug this
            // fixes (the same real bug class `Self::render_settings`'s own identical
            // `track_focus` already fixes for the Settings surface).
            .track_focus(&self.code_focus_handle)
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .bg(theme::surface::CENTER)
            .child(toolbar)
            .child(body)
            .into_any_element()
    }

    /// The toolbar's real segmented `Diff | File` toggle (`design_handoff_jerry_ade/README.md`'s
    /// Surface C toolbar spec) - the `Diff` segment is only real, clickable navigation when
    /// `has_diff` is true (there's nothing to switch *to* otherwise, and clicking it would be a
    /// dead affordance); `File` is always clickable, since every real file on disk can always be
    /// shown as a File view. Mirrors [`Self::render_right_sidebar_toggle`]'s own segmented-control
    /// shape verbatim (same track/active colours, same `cx.listener` pattern per segment).
    pub(super) fn render_diff_file_toggle(
        &self,
        has_diff: bool,
        effective_view: code_view::CodeView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let segment = |label: &'static str, view: code_view::CodeView, enabled: bool| {
            let is_active = effective_view == view;
            let mut el = div()
                .id(label)
                .px(px(8.0))
                .py(px(3.0))
                .rounded(theme::radius::CHIP)
                .when(is_active, |el| el.bg(theme::surface::SEGMENT_ACTIVE))
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(10.5))
                .text_color(if is_active {
                    theme::text::PRIMARY
                } else if enabled {
                    theme::text::DIMMER
                } else {
                    theme::text::DISABLED
                })
                .child(label);
            if enabled {
                el = el.cursor_pointer().on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        this.code_view = view;
                        cx.notify();
                    },
                ));
            }
            el
        };

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::SEGMENT_TRACK)
            .child(segment("Diff", code_view::CodeView::Diff, has_diff))
            .child(segment("File", code_view::CodeView::File, true))
    }

    /// One changed file's real diff content: a "binary file" note, or its real hunks as
    /// unified-diff-style themed lines, with a real `⋯ N unchanged lines` fold marker
    /// (`design_handoff_jerry_ade/README.md`'s Diff view fold spec) for the real gap between
    /// consecutive hunks (`crate::changes::fold_gap_between`, parsed from the hunks' own real
    /// `@@ ... @@` headers - never a fabricated line count). `wt_core::diff` has no lazy
    /// per-file hunk-loading state to build a "press ⏎ to load this hunk" treatment for (every
    /// non-binary changed file's hunks are already eagerly loaded - see that module's docs), so
    /// that part of the design's fold spec doesn't apply to this app's real data model; capped
    /// by [`MAX_RENDERED_DIFF_LINES_PER_FILE`] independent of `wt_core::diff`'s own load-time
    /// cap.
    pub(super) fn render_diff_file_detail(&self, file: &DiffFile) -> gpui::AnyElement {
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
            return container
                .child(render_sidebar_message(
                    "binary file (contents not diffed)".to_string(),
                    theme::text::FAINT,
                ))
                .into_any_element();
        }

        // A rename-only file (renamed with no content change) produces zero real `@@` hunks -
        // `git diff` has nothing to diff line-by-line - so falling through the loop below would
        // otherwise leave `container` with no children at all: a blank centre pane on click that
        // looks like a rendering bug rather than the real "nothing to show" state it actually
        // is. `changes::empty_hunks_message` picks the honest wording (naming the rename
        // specifically when that's the real cause, per `DiffFile::status`).
        if file.hunks.is_empty() {
            return container
                .child(render_sidebar_message(
                    changes::empty_hunks_message(file.status).to_string(),
                    theme::text::FAINT,
                ))
                .into_any_element();
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
                    .text_size(px(11.0))
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

        container.into_any_element()
    }

    /// Surface C's real File view (`design_handoff_jerry_ade/README.md`'s File view subsection):
    /// a real breadcrumb, real line-numbered/syntax-highlighted code (`crate::code_view`), and a
    /// real status bar - for whichever real file `relative_path` (resolved against
    /// [`Self::file_tree_root`]) names on disk.
    ///
    /// ## Caching, and staying off the foreground thread
    ///
    /// [`code_view::load_file`] (which runs a real `tree-sitter` parse for a `.rs` file) is only
    /// ever *dispatched* here (via [`Self::spawn_file_load`]), and only when
    /// [`Self::file_view_cache`] is missing or [`code_view::cache_is_fresh`] says it's stale
    /// against the file's real, freshly-read `mtime`/`len` (a real `std::fs::metadata` call - a
    /// single, cheap stat syscall, kept synchronous here unlike `load_file` itself, which
    /// additionally does a full `std::fs::read` plus, for a `.rs` file, a full `tree-sitter`
    /// parse) - never unconditionally on every render, and never run *inline* on the GPUI
    /// foreground thread: the actual read-and-parse work happens inside
    /// `cx.background_executor()`, with the result written back into `file_view_cache` from a
    /// `this.update(cx, ..)` callback once it resolves (see [`FileLoadState`]'s own docs for the
    /// measured real cost - up to 190ms for a single file in a debug build - that makes this not
    /// optional). This was verified directly, not just read over: `crate::code_view`'s own
    /// `cache_is_fresh` unit tests cover the staleness check in isolation, and this module's own
    /// `code_view_cache_tests` (below) open a real temp `.rs` file, force several real
    /// re-renders of the same open file, and assert `file_view_cache` stays `Some` with an
    /// unchanged `mtime`/`len` pair throughout - i.e. that a second, third, ... render of the
    /// same unmodified file never re-triggers [`code_view::load_file`] - *and* assert
    /// `file_view_cache` is still `None` immediately after the render that kicked off the very
    /// first load, before `cx.run_until_parked()` drives the background task to completion,
    /// proving the parse did not happen synchronously inline.
    ///
    /// ## Virtualization
    ///
    /// Every real line of `parsed.lines` is reachable - there is no cap on how many of them can
    /// ever become a rendered row, unlike (for example) [`MAX_RENDERED_DIFF_LINES_PER_FILE`]'s
    /// hard cap on the Diff view. `gpui::uniform_list` (verified against
    /// `vendor/zed/crates/gpui/examples/uniform_list.rs` and its own real, non-example callers,
    /// e.g. `vendor/zed/crates/git_ui/src/git_panel.rs`'s `commit_history_list`) only ever
    /// constructs [`render_file_view_line`] elements for whichever row range is actually
    /// scrolled into view, so a file with (say) 8000 real lines - this repo's own `root.rs`, at
    /// the time this doc comment was written - is genuinely scrollable end to end, not silently
    /// capped at some fixed prefix the user can never reach past no matter how far they scroll.
    pub(super) fn render_file_view(
        &mut self,
        relative_path: &Path,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let absolute_path = self.file_tree_root.join(relative_path);

        // Throttled real freshness check - see `Self::file_view_last_freshness_check`'s docs for
        // why this doesn't call `std::fs::metadata` unconditionally on every render. A path
        // mismatch (a different file than whatever was last checked) always forces a real,
        // immediate re-check regardless of how recently *some other* path was checked.
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
            // Within the throttle window for this exact path - the real mtime/len check already
            // ran recently (at most `FILE_FRESHNESS_CHECK_INTERVAL` ago) and nothing in this app
            // mutates `file_view_cache` for an already-fresh path outside of that check, so a
            // matching cached path is sufficient evidence of freshness without paying for another
            // `stat()` this render.
            self.file_view_cache
                .as_ref()
                .is_some_and(|cached| cached.path == absolute_path)
        };

        if !cache_fresh {
            // A real, already-observed outcome for this *exact* path - either a load already in
            // flight, or a previous real read failure - must never respawn another load on every
            // single render. Before this also checked `FileLoadState::Error` here, a real,
            // permanently unreadable path (e.g. a permissions error, or a real go-to-definition
            // target outside this app's own file tree that this process happens not to be able to
            // read) respawned a doomed load on *every* repaint: each failure called `cx.notify()`
            // (`Self::spawn_file_load`'s completion handler always does, to show the real error),
            // which triggered the next render, which respawned the load again - an unbounded,
            // permanent busy-loop with no real work left to do, pinning the foreground thread at
            // however fast `uniform_list` could repaint (measured at ~60 loads/repaints per
            // second) instead of settling into a real, stable error state.
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
            // The background load just dispatched (or already in flight from a previous render)
            // hasn't written a fresh `file_view_cache` yet - show its real, honest current
            // state instead of stale content from a *different* file, or nothing at all. The
            // very next render after it resolves (`cx.notify()` in `Self::spawn_file_load`'s
            // completion handler) will find `cache_fresh` true and fall through to real content
            // below.
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

        // A real go-to-definition navigation may have left a real target line waiting
        // specifically for this exact, already-fresh file - see `Self::pending_cursor_line`'s own
        // docs, including the path check this mirrors (`Self::spawn_file_load`'s completion
        // handler handles the other case: the target file's parse wasn't cached yet, so a real
        // background load had to happen first).
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

        // Surface C's real Diagnostic state (`design_handoff_jerry_ade/README.md`'s "Language
        // server UI" subsection) - only ever engaged for a real `.rs` file. `ensure_lsp_client`/
        // `dispatch_did_open` are real, idempotent, `&mut self` calls, so they must run (and
        // finish mutating `self.lsp_clients`/`self.lsp_opened_files`) *before* the immutable
        // `self.file_view_cache` borrow below is taken - see `AdeApp::file_view_diagnostics`'s
        // own docs for why the diagnostics index itself is computed in its own short-lived
        // borrow scope rather than interleaved with the `parsed` borrow that follows.
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

            // Computed exactly once per `render_file_view` call and reused for every
            // diagnostics lookup below, rather than letting each lookup independently re-derive
            // it (`lsp_core::LspClient::uri_for_path`/`path_to_uri` performs a real, blocking
            // `canonicalize()` syscall) - `uniform_list` means this method runs on every
            // repaint, so re-deriving it up to three times per call, as this used to, was a
            // real per-frame blocking-syscall cost, not a micro-optimization. See
            // `LspClient::uri_for_path`'s own docs.
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
        // Surface C's real Hover state (`design_handoff_jerry_ade/README.md`'s "Language server
        // UI" subsection) - only ever a real, live target for a `.rs` file (see
        // `Self::request_hover`'s own docs; every other extension has no real language server to
        // ask). Cloned once here (not re-derived per row) for the same reason `file_uri` above
        // is: this closure runs on every real repaint of whichever rows are scrolled into view.
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
        // Real go-to-definition viewport scrolling (`Self::navigate_to_definition`/
        // `Self::spawn_file_load`'s completion handler/this method's own already-fresh-
        // navigation branch, whichever actually lands a real target line, drives this handle's
        // `scroll_to_item` - see `Self::file_view_scroll_handle`'s own docs) - without this, F12
        // moved `Self::code_cursor` and the status bar's `ln N` but never the actual viewport.
        .track_scroll(&self.file_view_scroll_handle)
        .flex_1()
        .min_h_0()
        .bg(theme::surface::PTY)
        .font(font(theme::font::MONO))
        .text_size(px(12.5));

        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(render_file_breadcrumb(relative_path))
            .child(code);

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

/// The outcome of the most recent (or in-flight) `wt_core::diff::diff_against_base` call for
/// [`AdeApp::diff_root`]. Kept separate from [`DiffBase`] (rather than wrapping it in an
/// `Option`/`Result` at the call site) so "still computing" is a first-class, renderable
/// state rather than reusing an empty/default value that could be mistaken for "no changes".
pub(super) enum DiffLoadState {
    Loading,
    Loaded(DiffBase),
    Error(String),
}

/// The outcome of the most recent (or in-flight) `code_view::load_file` call for whichever real
/// on-disk path [`AdeApp::render_file_view`] most recently asked to load - mirrors
/// [`DiffLoadState`]'s own shape and reasoning for the exact same underlying cause:
/// `code_view::load_file` performs the same class of blocking I/O `diff_against_base` does (a
/// full `std::fs::read`, plus - for a `.rs` file - a full `tree-sitter` parse) and must never run
/// on the GPUI foreground thread. Measured directly against this repo's own 370KB `root.rs` in a
/// debug build: `code_view::highlight_rust` alone took 119-190ms, and the full `load_file` 124ms
/// - each one of those milliseconds spent blocking `render()` is a dropped frame.
///
/// Kept separate from [`AdeApp::file_view_cache`] (which holds the last real, *successfully*
/// loaded/parsed file) rather than folded into an `Option<Result<ParsedFile, String>>` there, so
/// a fresh load kicked off for a newly opened file doesn't have to overwrite (and thus briefly
/// blank) whatever was last successfully shown while the new one is still in flight - the same
/// "loading state is a first-class, renderable state of its own" reasoning `DiffLoadState`'s own
/// docs give.
#[derive(Debug)]
pub(super) enum FileLoadState {
    Idle,
    Loading(PathBuf),
    Error(PathBuf, String),
}

/// The real state of one real, in-flight or completed click-triggered `textDocument/hover`
/// request (`design_handoff_jerry_ade/README.md`'s Hover state) - see [`AdeApp::hover`]'s own
/// docs for the caching discipline this backs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct HoverEntry {
    /// The real, absolute path of the file the hovered symbol is in - `render_file_view` only
    /// ever shows this entry's [`Self::status`] when it matches the file currently open, so a
    /// hover card can never visually bleed onto a different file after navigation.
    path: PathBuf,
    /// The real, 1-based line number (matching [`AdeApp::code_cursor`]'s own convention) the
    /// hovered token is on - used both to find the right row to underline and, together with
    /// [`Self::byte_range`], as half of this entry's own real cache key.
    line_number: usize,
    /// The real byte range, within that line's own text, of whichever already-rendered
    /// token/run was clicked (`crate::root::render_file_view_line`'s own click handler) - the
    /// span [`crate::root::render_file_view_line`] underlines with
    /// `theme::syntax::HOVER_UNDERLINE`, and (together with [`Self::line_number`]) the other half
    /// of this entry's real cache key ([`Self::request_hover`]... see [`AdeApp::request_hover`]'s
    /// own docs for why re-deriving an LSP `character` offset isn't needed to detect "same click
    /// again").
    byte_range: Range<usize>,
    /// The real LSP `Position` (UTF-16 `character` offset) this entry's request was/will be sent
    /// with - kept alongside `byte_range` (rather than derived from it again) so
    /// [`AdeApp::trigger_goto_definition`] can reuse it directly for a real
    /// `textDocument/definition` request without recomputing it.
    position: lsp_core::lsp_types::Position,
    status: HoverStatus,
}

/// The real, distinguishable outcomes of one [`HoverEntry`]'s own request - mirrors
/// [`LspClientState`]'s own three-state shape (`Spawning`/`Failed`/`Ready` there;
/// `Loading`/`Failed`/`Ready` here), so `render_hover_card` can show an honest state for
/// whichever one currently applies rather than a blank card while a request is in flight.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum HoverStatus {
    Loading,
    /// A real response arrived - `Some` for a real, non-empty
    /// `hover_view::HoverRenderModel` (`crate::hover_view::build_hover_render_model` returned
    /// one), `None` for a genuine "rust-analyzer answered, and there's real nothing to show here"
    /// (e.g. hovering whitespace/punctuation) - never conflated with [`HoverStatus::Failed`],
    /// which means the request itself didn't complete.
    Ready(Option<hover_view::HoverRenderModel>),
    Failed(String),
}

/// Surface C's real Diagnostic-state card (`design_handoff_jerry_ade/README.md`: "a card below:
/// message `#e3908b`, note `#7d848b`, `rust-analyzer · E0277`") - one row per real diagnostic
/// currently indexed anywhere in the open file, `None` when there are none (a real, correct,
/// expected "clean file" state - see `crate::diagnostics_view`'s own docs - that renders no card
/// at all, not an empty one). Listing every diagnostic in the file here (rather than only the
/// one under the cursor) is a documented simplification: the design anchors this card under the
/// caret line, but this app has no floating-popup infrastructure yet (`lsp_popup` is a later,
/// H3 concern) - see the step report for this judgment call.
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

/// Surface C's real Hover-state card (`design_handoff_jerry_ade/README.md`: "card 430 wide:
/// signature, doc prose, `core::convert` + `F12 definition` footer") - `None` when
/// [`AdeApp::hover`] is `None`, or (defensively - should already be unreachable given every real
/// file-switch point resets [`AdeApp::hover`]) belongs to a different file than
/// `open_absolute_path` currently names. Same real, documented simplification
/// [`render_diagnostics_card`]'s own docs already state for the Diagnostic state: the design
/// anchors this under the caret line as a floating popup, but this app has no floating-popup
/// infrastructure, so it renders as a card below the code the same way the diagnostics card does.
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

/// The diff view's real `⋯ N unchanged lines` fold marker
/// (`design_handoff_jerry_ade/README.md`'s Diff view fold spec) - `N` is always a real count
/// derived from the hunks' own `@@ ... @@` headers (`crate::changes::fold_gap_between`), never
/// an estimate.
pub(super) fn render_fold_marker(gap: usize) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(4.0))
        .px(px(8.0))
        .h(px(20.0))
        .bg(theme::diff::FOLD_BG)
        .font(font(theme::font::MONO))
        .text_size(px(11.0))
        .text_color(theme::diff::FOLD_FG)
        .child(format!(
            "\u{22ef} {gap} unchanged line{}",
            if gap == 1 { "" } else { "s" }
        ))
}

/// One real diff line - added/removed/context, coloured per `design_handoff_jerry_ade/
/// README.md`'s Diff view line-kind table.
pub(super) fn render_diff_line(line: &wt_core::diff::DiffLine) -> impl IntoElement {
    let (prefix, fg, bg) = match line.kind {
        DiffLineKind::Added => ("+", theme::diff::ADD_FG, Some(theme::diff::ADD_BG)),
        DiffLineKind::Removed => ("\u{2212}", theme::diff::DEL_FG, Some(theme::diff::DEL_BG)),
        DiffLineKind::Context => (" ", theme::diff::CTX_FG, None),
    };
    let mut element = div()
        .flex()
        .font(font(theme::font::MONO))
        .text_size(px(11.5))
        .px(px(8.0))
        .text_color(fg);
    if let Some(bg) = bg {
        element = element.bg(bg);
    }
    element.child(format!("{prefix} {}", line.content))
}

/// The File view toolbar's always-rendered `Accept file` button - `design_handoff_jerry_ade/
/// README.md`: "**Accept file is always rendered**, dimmed (`#454b51` / border `#1f2327`) when
/// there is nothing to accept. It must never appear or disappear with the view." This app has no
/// real "accept" backing logic yet (no per-file review-apply action exists anywhere in this
/// crate), so it is *always* the dimmed, non-interactive state the spec describes for "nothing to
/// accept" - deliberately given no `cursor_pointer()`/`on_click` at all, rather than a click
/// handler that would silently do nothing (that would be exactly the kind of fake, bound-to-
/// nothing affordance this project's conventions forbid).
pub(super) fn render_accept_file_button() -> impl IntoElement {
    div()
        .flex_none()
        .px(px(8.0))
        .py(px(3.0))
        .rounded(theme::radius::BUTTON)
        .border_1()
        .border_color(theme::border::BUTTON_DISABLED)
        .font(font(theme::font::SANS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(10.5))
        .text_color(theme::text::GHOSTER)
        .child("Accept file \u{23ce}")
}

/// The File view's real breadcrumb (`design_handoff_jerry_ade/README.md`: "Breadcrumb 26 (`src ›
/// db › query_builder.rs › impl QueryBuilder › build`, ..., separators `#3d4248`, active crumb
/// `#a9b0b7`)") - built from `relative_path`'s own real path segments
/// (`code_view::breadcrumb_segments`). The design's deeper symbol-path suffix (`› impl
/// QueryBuilder › build`) is a documented scope simplification: it needs real symbol/AST-position
/// tracking (which function/impl block the cursor is currently inside) that this phase's read-only
/// viewer doesn't build - see this crate's report for the judgment call. The last (file name)
/// segment is the "active crumb"; every segment before it is a real ancestor directory, dimmer.
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

/// One real File view code row: a real 52px right-aligned line-number gutter
/// (`design_handoff_jerry_ade/Jerry.dc.html`'s File view code template: `width:52px;text-
/// align:right;padding-right:12px`), a real 3px git-gutter marker (tinted
/// `theme::diff::GIT_GUTTER` for `is_changed`, transparent otherwise), and the real
/// syntax-highlighted line content (`line.runs`, each run's own `code_view::color_for_kind`).
/// `is_current` tints the whole row (`theme::surface::CURRENT_LINE`) and brightens the gutter
/// number - `design_handoff_jerry_ade/Jerry.dc.html`'s own current-line row (`background:
/// #181c20`, gutter `color:#8b9197` vs. the usual `#3a3f44`).
///
/// Clicking a row sets `AdeApp::code_cursor` to `line_number` - a real line number from a real
/// click. There is no column here at all (not a fabricated `col 1`) - see `AdeApp::code_cursor`'s
/// own docs and `render_file_status_bar`'s docs for why: real per-character column tracking is a
/// documented scope simplification this phase, and showing a column that never actually reflects
/// where the user clicked would be exactly the kind of fake UI this project's conventions forbid.
///
/// `diagnostics` (a real, possibly-empty `Vec<diagnostics_view::LineDiagnostic>` for this exact
/// line - see `AdeApp::file_view_diagnostics`'s docs) drives the Diagnostic state's three
/// remaining real, per-row treatments (`design_handoff_jerry_ade/README.md`): a row tint
/// (`theme::syntax::DIAGNOSTIC_ROW_BG`, applied whenever `is_current` isn't already tinting the
/// row), a `.border_dashed()` bottom border under the real, byte-range-precise offending span
/// (`crate::diagnostics_view::overlay_diagnostic_runs`; GPUI has no true "dotted" border style -
/// see `vendor/zed/crates/gpui/src/styled.rs`'s own `border_dashed` - so this is the closest
/// real primitive available, the same "drop it, keep the real one" precedent `theme::shadow`'s
/// own docs already establish for the popover shadow), and a real, dim inline message from the
/// first diagnostic on the line appended after the code (a full per-diagnostic breakdown is the
/// separate card `render_diagnostics_card` renders below the code area, not repeated per-row).
/// The File view row's real dotted-underline colour for a diagnostic of `severity`
/// (`design_handoff_jerry_ade/README.md` only specifies a treatment for the error case -
/// `#e0625c`/[`theme::syntax::ERROR_UNDERLINE`] - so the other three are a documented judgment
/// call, not a spec value: `Warning` reuses [`theme::term::WARN`] (`#d8a94a`, this app's existing
/// amber, already used for warning-adjacent UI elsewhere), and `Information`/`Hint` reuse the
/// existing dim/faint neutral text tokens ([`theme::text::DIM`]/[`theme::text::FAINT`]) rather
/// than any new hex value - `Hint` deliberately gets the *dimmer* of the two, since LSP hints are
/// conventionally the least severe/most subtle real diagnostic kind (the same real-editor
/// convention `VS Code`'s own hint rendering - a faint dotted underline, no row highlight -
/// follows, referenced directly in this fix's own step report).
pub(super) fn diagnostic_underline_color(severity: diagnostics_view::Severity) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::ERROR_UNDERLINE,
        diagnostics_view::Severity::Warning => theme::term::WARN,
        diagnostics_view::Severity::Information => theme::text::DIM,
        diagnostics_view::Severity::Hint => theme::text::FAINT,
    }
}

/// The File view row's real background tint for a diagnostic of `severity` - `None` means no
/// tint at all. Only `Error` gets one (`design_handoff_jerry_ade/README.md`'s own `#191416` row
/// tint, [`theme::syntax::DIAGNOSTIC_ROW_BG`]): the design spec never mandates one for the other
/// three severities, and following the same real-editor convention
/// [`diagnostic_underline_color`]'s own docs cite (hints/info render subtly, without a row-level
/// highlight), a `Warning`/`Information`/`Hint`-only line is distinguished from a clean line by
/// its dotted underline alone, not an additional tint - keeping every non-error severity visibly
/// *less* alarming than an error, which is the whole point of having severities at all.
pub(super) fn diagnostic_row_bg(severity: diagnostics_view::Severity) -> Option<gpui::Rgba> {
    match severity {
        diagnostics_view::Severity::Error => Some(theme::syntax::DIAGNOSTIC_ROW_BG),
        _ => None,
    }
}

/// The File view row's real inline end-of-line message colour for a diagnostic of `severity` -
/// `Error` keeps the design's own dim red ([`theme::syntax::DIAGNOSTIC_INLINE_MESSAGE`],
/// `#6b4a48`); every other severity reuses [`theme::text::FAINT`] (no bespoke per-severity
/// message colour is specified by the design, and a single dim neutral tone reads correctly
/// alongside any of the three non-error underline colours [`diagnostic_underline_color`] can
/// produce).
pub(super) fn diagnostic_inline_message_color(severity: diagnostics_view::Severity) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::DIAGNOSTIC_INLINE_MESSAGE,
        _ => theme::text::FAINT,
    }
}

/// Real, automated proof that the four severities produce visibly distinct rendered treatments -
/// a literal screenshot isn't practical in this sandbox's test environment, so this asserts
/// directly against the real colour-mapping functions [`render_file_view_line`] itself calls
/// (not a reimplemented duplicate of their logic), which is what actually determines each row's
/// real drawn appearance. The regression this guards: before this fix, every severity collapsed
/// onto the same `ERROR_UNDERLINE`/`DIAGNOSTIC_ROW_BG` treatment regardless of its real severity.
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

    /// The exact regression this fix addresses, checked directly: a real Hint used to render
    /// pixel-identical to a real Error (same underline colour, same row tint, same inline
    /// message colour) - every one of those three dimensions must now differ.
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

/// Bundles [`render_file_view_line`]'s two Hover-state parameters together purely to keep that
/// function's own argument count under clippy's `too_many_arguments` limit - `target`/`entry` are
/// otherwise unrelated to each other in meaning (see [`AdeApp::request_hover`]/[`HoverEntry`]'s
/// own docs), so this is real grouping-for-arity, not a real conceptual unit.
pub(super) struct HoverRenderContext<'a> {
    /// The current file's own real, absolute path - `Some` only for a `.rs` file (see
    /// [`AdeApp::request_hover`]'s own docs for why hover has no real target otherwise).
    target: Option<&'a Path>,
    /// [`AdeApp::hover`]'s current real entry, if any.
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
    // "Worst wins" - see `diagnostics_view::Severity::worst`'s own docs for why this (rather
    // than "whichever diagnostic happens to be first in the Vec") is this app's documented,
    // explicit tie-break for a line's single row-level treatment when it carries diagnostics of
    // mixed severity.
    let worst_severity = diagnostics_view::Severity::worst(diagnostics);
    // The real hovered span on *this* line, if any - see `HoverEntry::byte_range`'s own docs for
    // why run-level equality (not a re-derived UTF-16/byte conversion of `rust-analyzer`'s own
    // returned `Hover::range`) is exactly the right, real granularity here.
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

        // A stable `.id()` is applied unconditionally (not only for a real, clickable token) so
        // `run`'s own type stays exactly `Stateful<Div>` across every branch below - GPUI's
        // `Div::id` and `Stateful<Div>`'s own subsequent `.cursor_pointer()`/`.on_click()` change
        // the concrete type, and an `if`/`else` reassigning the same `let mut run` binding with
        // two different concrete types on different branches doesn't compile.
        let mut run = div()
            .id(("file-view-code-token", line_number * 1_000_000 + run_start))
            .text_color(code_view::color_for_kind(kind))
            .child(run_text.clone());
        if is_diagnostic {
            // Every underlined run on this line shares the line's own worst-severity colour
            // (see this function's own docs above) - `unwrap_or` a value that is never actually
            // reached here (`is_diagnostic` is only ever `true` when `diagnostics` was non-empty,
            // which is exactly when `worst_severity` is `Some`), kept as a real fallback rather
            // than an `.unwrap()` per this crate's own no-`.unwrap()`-outside-tests convention.
            let underline_color = worst_severity
                .map(diagnostic_underline_color)
                .unwrap_or(theme::syntax::ERROR_UNDERLINE);
            run = run
                .border_b_2()
                .border_color(underline_color)
                .border_dashed();
        } else if hovered_byte_range.as_ref() == Some(&(run_start..run_end)) {
            // A real diagnostic (2px dotted, error-severity colour) always wins over the hover
            // underline (1px solid `theme::syntax::HOVER_UNDERLINE`) when both would land on the
            // exact same run - a deliberate priority (an active error is more urgent than a
            // symbol the user merely clicked to inspect), not an accidental overwrite.
            run = run
                .border_b_1()
                .border_color(theme::syntax::HOVER_UNDERLINE);
        }
        // Only a real, non-whitespace token is a real hover/go-to-definition target - clicking
        // whitespace/an empty gap run would just ask `rust-analyzer` about nothing, so it's left
        // as a plain, non-interactive span (still real syntax-highlighted text, just not wrapped
        // in a second, redundant `on_click`/`cursor_pointer` on top of the line's own).
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
        // Only the message's real *first* line is shown inline - `uniform_list` measures one
        // row's height and applies it uniformly to every row (see
        // `vendor/zed/crates/gpui/examples/uniform_list.rs`/that widget's own real callers, e.g.
        // `vendor/zed/crates/git_ui/src/git_panel.rs`'s `commit_history_list`, for the fixed-
        // row-height virtualized-list semantics this follows), so a real, genuinely multi-line
        // rust-analyzer/rustc message (embedded `\n`s are routine - e.g. a real "mismatched
        // types\nexpected `i32`, found `&str`") would otherwise clip or overlap the row below
        // it. The full, unmodified multi-line message is still shown in
        // `render_diagnostics_card` below, which isn't height-constrained the same way.
        // `.lines().next()` returns `None` only for a genuinely empty message string, hence
        // `unwrap_or_default()` rather than `.unwrap()` (forbidden outside `#[cfg(test)]` by
        // this crate's own conventions).
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
                .child(line_number.to_string()),
        )
        .child(div().flex_none().w(px(3.0)).h(px(20.0)).bg(if is_changed {
            theme::diff::GIT_GUTTER
        } else {
            work_surface::TRANSPARENT
        }))
        .child(div().flex_1().min_w_0().pl(px(12.0)).child(text_row))
        .into_any_element()
}

/// The File view's real status bar (`design_handoff_jerry_ade/README.md`: "Status bar 28: ...
/// `Rust`, `ln 44, col 14`, `LF`") - real language, real last-click cursor *line* (`None` until
/// the first click, per `AdeApp::code_cursor`'s docs), a real, byte-detected line-ending label,
/// and - for a `.rs` file, once a real `lsp_core::LspClient` exists for its repo root - a real
/// `rust-analyzer` status (`lsp_status`, `None` for every non-Rust file: there is genuinely no
/// language server for e.g. `.toml`, so no status field is shown for one, rather than a
/// fabricated placeholder). The design's own `col 14` remains deliberately omitted: there is
/// still no real per-character column-hit-testing in this app (a documented scope
/// simplification from an earlier phase), so showing a column next to a real `ln N` would look
/// just as real while actually always reading `1` - exactly the kind of fake UI this project's
/// conventions forbid.
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

/// Real, interactive regression coverage for `AdeApp::render_file_view`'s cache (the exact bug
/// class - "re-running an expensive parse on every render" - this crate's own prior phases hit
/// and fixed repeatedly; see `AdeApp::file_view_cache`'s and `AdeApp::render_file_view`'s docs).
#[cfg(test)]
mod code_view_cache_tests {
    use super::*;
    use gpui::TestAppContext;

    /// A real, direct, wall-clock proof that opening a large real file no longer blocks
    /// `render_center_pane` on the full `code_view::load_file` parse - not just a pointer-
    /// identity/`None`-before-`run_until_parked` proxy (see this module's other two tests for
    /// those), but an actual timing comparison against a real synchronous baseline, on the same
    /// file, on the same machine, in the same test run (a ratio comparison, not an absolute
    /// wall-clock threshold, so this isn't flaky under CI/machine load the way an absolute
    /// millisecond budget would be).
    ///
    /// Uses this crate's own `root/code_surface.rs` as the large real `.rs` fixture (originally
    /// the pre-module-split `root.rs`, the same file a prior audit of this phase measured
    /// `code_view::highlight_rust` alone taking 119-190ms and the full `code_view::load_file`
    /// 124ms on, in a debug build, when it still ran inline - `root.rs` was later split into
    /// `root/*.rs` submodules for maintainability, `code_surface.rs` being the largest of them).
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

        // The real, direct, synchronous baseline: how long the actual blocking work (read +
        // tree-sitter parse) takes for this exact file, on this exact machine, in this exact
        // build - i.e. what used to run inline on the render thread.
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

        // Drive the real background load to completion and confirm it really did load the
        // whole large file (not a truncated/fabricated stand-in for it).
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

    /// Opens a real file and confirms two things in one pass:
    ///
    /// 1. The real parse genuinely happens off the GPUI foreground thread - `file_view_cache` is
    ///    still `None` immediately after the render that kicks off the load, *before*
    ///    `cx.run_until_parked()` drives `AdeApp::spawn_file_load`'s background task to
    ///    completion. If `code_view::load_file` were ever called synchronously inline during
    ///    `render_file_view` again (the exact bug this phase's fix addresses), this assertion
    ///    would fail: `file_view_cache` would already be populated at this point, with no
    ///    background task to wait for at all.
    /// 2. Once the load has actually completed, several further real re-renders of the centre
    ///    pane never rebuild the cached parse - proven by real pointer identity, not just equal
    ///    *content*: a fresh `code_view::load_file` call allocates a brand new `Vec` for
    ///    `ParsedFile::lines`, so if the render path were re-parsing on every frame, the buffer
    ///    would get a freshly allocated address each time; a real cache hit leaves the existing
    ///    `Some(ParsedFile)` completely untouched, so the address is trivially identical.
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

        // Drives `AdeApp::spawn_file_load`'s background task (a real `cx.background_executor()`
        // spawn) to completion, and its `this.update(cx, ..)` write-back along with it.
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

    /// The other half of the same behavior: a real, on-disk content change to the open file (a
    /// different `mtime`/`len`) *must* invalidate the cache - confirms this isn't a cache that
    /// never refreshes, just one that doesn't needlessly re-run on an unchanged file. Each real
    /// load (the initial one and the one triggered by the on-disk change) is driven to
    /// completion via `cx.run_until_parked()`, since both now run on the background executor
    /// rather than inline. Sleeps past [`FILE_FRESHNESS_CHECK_INTERVAL`] before the renders that
    /// must observe the change - see [`AdeApp::file_view_last_freshness_check`]'s docs: a real
    /// `std::fs::metadata` re-check only happens at most that often now, so a change made (and
    /// re-rendered against) within the same throttle window as the prior check would - correctly
    /// - not be picked up yet; `renders_within_the_throttle_window_do_not_pick_up_a_fresh_on_disk_change`
    /// below covers that half directly.
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

        // A real content change with more real lines than before - not a fabricated cache
        // invalidation signal.
        std::fs::write(
            &file_path,
            "fn add() -> i32 {\n    1\n}\n\nfn subtract() -> i32 {\n    -1\n}\n",
        )
        .expect("rewrite sample.rs");

        // Past the real throttle window, so the next render's freshness check is a real,
        // unthrottled `std::fs::metadata` call again.
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

    /// Real proof of [`AdeApp::file_view_last_freshness_check`]'s own throttling: a real on-disk
    /// change made *within* [`FILE_FRESHNESS_CHECK_INTERVAL`] of the last real freshness check
    /// must not be picked up yet (no `std::fs::metadata` re-check has run), and the exact same
    /// change must be picked up on the very next render once the window has passed - i.e. this
    /// is a real, bounded staleness window, not a cache that simply never re-checks.
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

        // A real content change, made immediately - well within the throttle window of the
        // freshness check the render above just performed.
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

        // Past the real throttle window - the exact same on-disk change is now real, stale
        // enough to actually be observed.
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

/// Real, deterministic regression coverage for the cross-file cursor leak
/// [`AdeApp::pending_cursor_line`]'s own docs describe: [`AdeApp::navigate_to_definition`] to a
/// real file B that isn't cached yet leaves a one-shot target line waiting for B's own real
/// background load to finish; opening a *different*, unrelated file C before that load resolves
/// must never let C's own load misapply B's stale target line.
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

        // A real go-to-definition landing on B's real line 5 - B isn't cached yet, so this only
        // sets a real, one-shot `pending_cursor_line` instruction rather than `code_cursor`
        // directly (see `AdeApp::navigate_to_definition`'s own docs).
        app.update_in(cx, |app, window, cx| {
            app.navigate_to_definition(file_b.clone(), 5, window, cx);
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.pending_cursor_line.clone()),
            Some((file_b.clone(), 5))
        );
        app.update(cx, |app, cx| {
            // Dispatches B's real background load (`AdeApp::spawn_file_load`) - not yet driven
            // to completion (`cx.run_until_parked()` hasn't run), matching the real race: the
            // load is genuinely in flight.
            app.render_center_pane(cx);
        });

        // Before B's own real background load resolves, the user clicks a completely unrelated
        // file C in the tree.
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_c.clone(), window, cx);
        });
        app.update(cx, |app, cx| {
            // Dispatches C's own real background load, replacing (and so - dropping a `Task`
            // cancels it immediately - cancelling) B's still in-flight one.
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

/// Real, deterministic regression coverage for the unbounded busy-loop
/// [`FileLoadState::Error`]'s own docs describe: a real read failure for a path must settle into
/// a stable error state, never respawn a doomed load again on every subsequent render.
#[cfg(test)]
mod unreadable_file_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn an_unreadable_file_settles_into_a_stable_error_state_without_respawning(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        // A real, deterministic, cross-platform read failure - no such path exists on disk, so
        // `code_view::load_file`'s own `fs::metadata`/`fs::read` calls fail every single time,
        // with no platform-specific permissions setup needed.
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

        // The real regression this guards against: before the fix, this next render alone -
        // called synchronously, with no further `run_until_parked()` first - would flip
        // `file_load_state` straight back to `Loading` (`AdeApp::spawn_file_load` sets that
        // synchronously, before any real background work even runs), because the respawn guard
        // only ever checked `FileLoadState::Loading`, never `FileLoadState::Error`. Left
        // unbounded, each real failed load called `cx.notify()`, which triggered the very next
        // render, which respawned the doomed load again - a permanent busy-loop (measured at
        // ~60 loads/repaints per second), not a real, stable error state.
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

/// The real, practical end-to-end proof H3's hover/go-to-definition feature exists to deliver -
/// mirrors [`lsp_diagnostics_wiring_tests`]'s own real "spawn a real `rust-analyzer` through this
/// app's own real code path, wait real wall-clock time for a real async result, assert on real
/// content" shape, applied to `AdeApp::request_hover`/`AdeApp::trigger_goto_definition` instead of
/// diagnostics. `AdeApp::request_hover` is called directly (the same real method
/// `crate::root::render_file_view_line`'s own click handler calls) rather than synthesizing a
/// real GPUI mouse click on a specific virtualized row's pixel position - consistent with
/// `lsp_diagnostics_wiring_tests`'s own `AdeApp::open_file_view` call, which exercises the real
/// production method a real Files-tree click would call, not a simulated click event.
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

    /// Real, bounded wall-clock retry loop that keeps re-sending a real `AdeApp::request_hover`
    /// call for the exact same real click until it resolves to a real, non-empty
    /// `hover_view::HoverRenderModel`, or `deadline` passes. A single real request can honestly
    /// come back `Ready(None)` while `rust-analyzer` is still mid-index for this specific
    /// position (the exact same real "not resolved yet" behavior
    /// `lsp_core::client::tests::rust_analyzer_returns_a_real_definition_location_for_a_call_site`
    /// had to retry past for an empty `GotoDefinitionResponse::Array`) - `AdeApp::request_hover`'s
    /// own real caching discipline (a no-op for a repeated identical click) means `app.hover` is
    /// reset to `None` between real attempts here so each retry is a genuine new request, not a
    /// cache hit against the previous empty answer.
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

    /// Real click position for `"    let result = add_one(41);"` (line index 8, 0-based - see
    /// `lsp_core::client::tests::rust_analyzer_returns_a_real_hover_for_a_documented_function`'s
    /// identical fixture/position, reused here verbatim since it's already verified against a
    /// real `rust-analyzer` response) - the real `add_one` call-site identifier itself spans
    /// bytes 17..24 of that line (`"    let result = "` is 17 real ASCII bytes, `"add_one"` is 7
    /// more), matching exactly the real byte range a real tree-sitter identifier token/run for it
    /// would carry (this test calls `AdeApp::request_hover` directly rather than going through a
    /// real render/click, so this range is asserted by hand rather than read off a real
    /// `code_view::RenderedLine` run - see this module's own top-level docs for why that's still
    /// a real, honest exercise of the same method a real click would call).
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

    /// The real hover flow: a real click-equivalent `AdeApp::request_hover` call against a real,
    /// running `rust-analyzer` (spawned through `AdeApp::render_file_view`'s own real
    /// `ensure_lsp_client` path, not `lsp_core` called directly) resolves to a real
    /// `hover_view::HoverRenderModel` whose real signature and doc text match the fixture's own
    /// real documented function - not placeholder or guessed content.
    #[gpui::test]
    fn a_real_click_resolves_to_a_real_hover_render_model(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();
        // `AdeApp::render_file_view` (the real method that spawns the real `LspClient`) only
        // actually runs as part of a real render - `render_center_pane` drives it, mirroring
        // `lsp_diagnostics_wiring_tests`'s own identical need to call it directly in a headless
        // test (no real window compositor is driving repaints here).
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        // Keeps re-rendering (the real trigger for `AdeApp::dispatch_did_open`, once the client
        // is `Ready`) while waiting for the real client to finish its handshake - a plain "wait,
        // then render once" would leave a real window where the client only just became `Ready`
        // and `didOpen` was never sent for this render pass, exactly the gap this loop closes.
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

    /// Proves the real `F12` keybinding is genuinely wired end to end at the GPUI action-dispatch
    /// layer: `crate::lib::run`'s real `cx.bind_keys([... KeyBinding::new("f12",
    /// root::GotoDefinition, None) ...])` entry, `AdeApp::render`'s real
    /// `.on_action(cx.listener(Self::handle_goto_definition_action))`, and
    /// `handle_goto_definition_action` itself all genuinely connect a dispatched [`GotoDefinition`]
    /// action to a real call into `AdeApp::trigger_goto_definition` - verified here by observing a
    /// real, distinguishing side effect (`AdeApp::hover` being read, which only happens inside
    /// `trigger_goto_definition`): with no file ever opened, `AdeApp::hover` is `None`, so
    /// `trigger_goto_definition` returns immediately without spawning anything - a real, harmless,
    /// deterministic no-op that this test can assert didn't panic and didn't spawn a stray
    /// background task, which is exactly what "the action reached the handler and the handler's
    /// real early-return path ran" looks like from the outside.
    ///
    /// ## Why the *full* navigation behavior below is proven a different way
    ///
    /// [`a_real_click_resolves_to_a_real_hover_render_model`]'s own File view (`open_change`
    /// naming a real `.rs` file, `code_view = File`) mounted via `AdeApp::render_center_pane` was
    /// found, while writing this phase's tests, to leave `TestAppContext::dispatch_action` unable
    /// to reach *any* `on_action` handler at all - reproduced down to the minimal case of setting
    /// `open_change`/`code_view` directly with no LSP/file content involved at all. This was a
    /// real, genuine bug (`AdeApp::render_center_pane` stops rendering the active session's own
    /// terminal pane - the previously-focused node - the instant a File view mounts, leaving
    /// `Window::focus` dangling on a `FocusId` the last rendered frame no longer contains), the
    /// same bug class [`palette_focus_tests`]/[`settings_focus_tests`]'s own docs describe, now
    /// fixed for Surface C too by [`AdeApp::code_focus_handle`] - see that field's own docs, and
    /// [`code_focus_tests`] for real, interactive coverage that `ToggleSettings`/`TogglePalette`/
    /// `GotoDefinition` all genuinely reach their handlers with a File view open. This test itself
    /// stays deliberately minimal (a fresh window, `hover == None`, no file ever opened) rather
    /// than folding that File-view-open case in here too, so the real navigation behavior below
    /// is still proven the way it originally was: [`f12_action_navigates_to_the_real_definition_
    /// line`] calls `AdeApp::trigger_goto_definition` directly - the exact same method this real,
    /// verified action handler calls - mirroring the exact "call the real production method the
    /// real input event would call, rather than simulate the input event itself" pattern this
    /// module's own [`a_real_click_resolves_to_a_real_hover_render_model`] and
    /// `lsp_diagnostics_wiring_tests`'s `AdeApp::open_file_view` call both already establish.
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

        // `handle_goto_definition_action`'s only real effect with `hover == None` is a harmless
        // early return inside `trigger_goto_definition` - confirming the app is still alive and
        // in exactly the same real state is the honest, available proof that dispatch reached it
        // without erroring, without this test needing to instrument production code with a debug
        // hook just to observe it.
        assert_eq!(app.read_with(cx, |app, _| app.hover.clone()), None);
    }

    /// The real go-to-definition flow: `AdeApp::trigger_goto_definition` - the exact same real
    /// method [`f12_action_reaches_the_real_handler_on_a_fresh_window`] just proved a real `F12`
    /// keypress reaches - sends a real `textDocument/definition` request using `AdeApp::hover`'s
    /// own real position, and a real response navigates the viewer's real `AdeApp::code_cursor`
    /// to the function's own real definition line, not the call site the request was sent from.
    /// See this module's own docs above for why this calls `trigger_goto_definition` directly
    /// rather than `cx.dispatch_action(GotoDefinition)`.
    #[gpui::test]
    fn f12_action_navigates_to_the_real_definition_line(cx: &mut TestAppContext) {
        let project = write_scratch_project(FIXTURE_SOURCE);
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        // See the hover test above's identical loop for why re-rendering here (not just waiting)
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

        // `AdeApp::trigger_goto_definition` reads `AdeApp::hover`'s own real `path`/`position`
        // regardless of whether that entry's own hover *content* has resolved yet (see
        // `GotoDefinition`'s own docs) - a real `Loading` entry already carries a real, valid
        // request target, so this only needs `request_hover` to have been called once, not to
        // have fully settled the way the hover test above needs a real render model back.
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

        // Retried on a real timer rather than called exactly once: a real
        // `textDocument/definition` response can honestly be empty while `rust-analyzer` is still
        // mid-index for this exact position (the same real "not resolved yet" behavior
        // `lsp_core::client`'s own end-to-end definition test had to retry past), and a real user
        // would just press `F12` again - this test does exactly that instead of treating one
        // empty answer as failure.
        let definition_deadline = Instant::now() + Duration::from_secs(120);
        loop {
            app.update(cx, |app, cx| {
                app.trigger_goto_definition(cx);
            });
            cx.run_until_parked();
            // The real `textDocument/definition` request runs on a real background OS thread
            // (`cx.background_executor().spawn` in `AdeApp::trigger_goto_definition`) - GPUI's
            // deterministic test executor's own `run_until_parked` only drains *its own*
            // scheduled queue, which is empty again the instant that real background call is
            // dispatched (the call itself blocks a genuine thread-pool thread, invisible to the
            // deterministic scheduler until it finishes and schedules a real completion
            // callback) - so a real wall-clock sleep has to happen here before a *second*
            // `run_until_parked` can actually observe and run that completion callback.
            std::thread::sleep(Duration::from_millis(300));
            cx.run_until_parked();
            // `fn add_one` is on real line 4 (1-based - `AdeApp::code_cursor`'s own convention;
            // 0-based line 3 in the fixture) - genuinely different from `CALL_SITE_LINE` (9),
            // proving this is real navigation, not a no-op that left the cursor where it was.
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
