//! The editable File view: one open file's rows, its breadcrumb, and its footer status
//! bar. The real text mutation behind a keystroke lives in `super::editing`; this module
//! only draws what `super::edit_buffer` currently holds.

use super::lsp_ui::{
    diagnostic_inline_message_color, diagnostic_row_bg, diagnostic_underline_color,
    render_diagnostics_card,
};
use super::zoom::zoom_scoped;
use super::*;
use crate::lsp::client::{lsp_file_status, LspFileStatus};
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_sidebar_message;

impl AdeApp {
    /// Surface C's File view: a breadcrumb, line-numbered/syntax-highlighted code
    /// (`crate::code_surface::code_view`), and a status bar for whichever file `relative_path` (resolved
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
    pub(in crate::code_surface) fn render_file_view(
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

        // The real, up-to-the-instant external-change-vs-unsaved-edit conflict (Revision R8.5a) -
        // see `AdeApp::file_external_conflict`'s own docs. Must run here, alongside the freshness
        // check itself and *before* the `!cache_fresh` early return below (which shows a "loading"
        // placeholder instead of reaching any of this function's later code this render) - a
        // dirty buffer's file going stale is exactly the `!cache_fresh` case, so detecting it only
        // reachable from the "already fresh" fall-through path below would make this dead code.
        //
        // Deliberately compared against the buffer's *own* `saved_mtime`/`saved_len` (the same
        // real authoritative basis `AdeApp::save_active_file`'s own check uses), not against
        // `cache_fresh`/`file_view_cache` above: `spawn_file_load` always refreshes
        // `file_view_cache` to match whatever's really on disk, *regardless of whether the edit
        // buffer is dirty* (it's the diagnostics/hover source of truth, not the editor) - so
        // `cache_fresh` becomes `true` again on the very next throttled check after that refresh
        // even while the buffer is still genuinely diverged from the new disk content. Basing
        // this on `cache_fresh` instead (an earlier version of this code did) let the conflict
        // banner silently self-clear a `FILE_FRESHNESS_CHECK_INTERVAL` or two after the external
        // change, while the real conflict - a dirty buffer that still doesn't match disk - was
        // still fully present; `save_active_file`'s own independent check meant this never risked
        // actual data loss, but the *visible warning* disappearing on its own would have been a
        // real, deceptive regression from "surface a real warning".
        if should_check {
            match self.edit_buffers.get(relative_path) {
                Some(buffer) if buffer.is_dirty() => {
                    let metadata = std::fs::metadata(&absolute_path).ok();
                    let disk_mtime = metadata.as_ref().and_then(|meta| meta.modified().ok());
                    let disk_len = metadata.as_ref().map(|meta| meta.len());
                    let unchanged_since_buffer_loaded =
                        disk_mtime == buffer.saved_mtime && disk_len == Some(buffer.saved_len);
                    if unchanged_since_buffer_loaded {
                        self.file_external_conflict.remove(relative_path);
                    } else {
                        self.file_external_conflict
                            .insert(relative_path.to_path_buf());
                    }
                }
                _ => {
                    self.file_external_conflict.remove(relative_path);
                }
            }
        }

        // GitHub issue #29: real, off-thread, cached inline git blame - a no-op call whenever
        // the setting is off or a fresh-enough cache entry already exists (see
        // `Self::maybe_refresh_blame`'s own docs), so this costs nothing extra on the common
        // "nothing changed since last render" path.
        self.maybe_refresh_blame(&absolute_path, cx);

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
                        theme::status::FAIL.into(),
                    )
                }
                _ => render_sidebar_message(
                    format!("loading {}...", absolute_path.display()),
                    theme::text::FAINT.into(),
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

        // Computed early (moved up from where it used to be derived, below) - Revision R8.5b's
        // diagnostics indexing needs it before `file_view_cache` is even guaranteed present.
        let relative_path_buf = relative_path.to_path_buf();

        // Diagnostics apply to any extension `crate::language`'s registry spawns a real LSP
        // client for (Rust/TypeScript-family/Python, and - as of this revision - Vue, which
        // genuinely spawns two coordinated processes; Go stays detection-only, see that module's
        // own docs). `ensure_lsp_client`/`dispatch_did_open` are
        // idempotent `&mut self` calls that must finish before the immutable `file_view_cache`
        // borrow below is taken.
        let extension = absolute_path.extension().and_then(|ext| ext.to_str());
        let language_id = language::lsp_language_id_for_extension(extension);
        let has_lsp = language_id.is_some();
        // Hoisted out of the block below (Revision R11 audit finding 2) so the exact same
        // already-computed primary binary name that keys `lsp_clients` also names the server in
        // this file's own footer - see `lsp_status_label`, which used to say "rust-analyzer" for
        // every language regardless.
        let lsp_binary = language::lsp_binary_for_extension(extension);

        let lsp_status = if let Some(language_id) = language_id {
            let repo_root = self.file_tree_root.clone();
            // Only a cheap, static registry lookup happens here on every repaint - the real,
            // possibly PATH-probing `ServerSpawnConfig` (e.g. Pyright's `pythonPath` resolution)
            // is built inside `ensure_lsp_client` itself, off the render thread, and only when a
            // spawn is actually needed (see that method's own docs for why this moved).
            let canonical_extension =
                language::entry_for_extension(extension).map(|entry| entry.extension);
            self.ensure_lsp_client(repo_root.clone(), canonical_extension, cx);
            let state = lsp_binary
                .and_then(|binary| self.lsp_clients.get(&(repo_root.clone(), binary)).cloned());
            // The companion's own independent lifecycle entry, for a language that has one (see
            // `crate::language::CompanionServer`) - `None` for every single-server language, which
            // keeps this whole block's behavior there exactly what it was.
            let companion_state = language::companion_for_extension(extension).and_then(|spec| {
                self.lsp_clients
                    .get(&(repo_root.clone(), spec.client_key))
                    .cloned()
            });
            // One resolved facade for this whole render pass - `didOpen` fan-out, the merged
            // diagnostics below, and the status line all read the same real view, so they can't
            // disagree about which processes are actually backing this file right now.
            let connection = self.lsp_connection_for_path(&absolute_path);
            // A `didOpen` is sent exactly once per real path (see `AdeApp::lsp_opened_files`), so
            // it must not go out while a companion is still mid-spawn: the connection would be a
            // `Single` at that moment, the primary alone would be opened, and the companion -
            // arriving `Ready` a moment later - would never be told about the file at all, which
            // live-reproduced as an entire real half of the diagnostics silently never appearing.
            // A companion that has genuinely `Failed` is never coming, so it is not waited on.
            let companion_still_spawning =
                matches!(companion_state, None | Some(LspClientState::Spawning))
                    && language::companion_for_extension(extension).is_some();
            if let Some(connection) = &connection {
                if !companion_still_spawning {
                    self.dispatch_did_open(
                        connection.clone(),
                        absolute_path.clone(),
                        language_id,
                        cx,
                    );
                }
            }

            // Computed once and reused below, since `uri_for_path` does a blocking
            // `canonicalize()` syscall and this method runs on every repaint.
            let file_uri = lsp_core::LspClient::uri_for_path(&absolute_path).ok();

            let diagnostics_map = match (&connection, &file_uri) {
                (Some(connection), Some(uri)) => {
                    let diagnostics = connection.diagnostics_for_uri(uri).unwrap_or_default();
                    // Real live tracking (Revision R8.5b): index against the *live* edit
                    // buffer's own `lines` when one exists for this file, not
                    // `file_view_cache.lines` (the last-*saved* snapshot). Now that
                    // `Self::schedule_lsp_sync` keeps the server's own document state in sync
                    // with real, live *unsaved* edits (not just what's on disk - see that
                    // method's own docs), a real diagnostic's line/character position is
                    // reported relative to that same live content. Indexing it against stale
                    // last-saved line boundaries would silently misplace it (or drop it
                    // outright - a diagnostic on a line that only exists post-edit has no
                    // matching last-saved line at all), the exact live-reproduced bug this fix
                    // closes. `file_view_cache`/`parsed.lines` is still the real fallback for a
                    // file with no edit buffer at all (still-loading, or truncated/non-UTF-8 and
                    // thus permanently read-only - see `EditBuffer`'s own docs).
                    match self.edit_buffers.get(&relative_path_buf) {
                        Some(buffer) => {
                            diagnostics_view::index_diagnostics_by_line(&diagnostics, &buffer.lines)
                        }
                        None => match self.file_view_cache.as_ref() {
                            Some(parsed) => diagnostics_view::index_diagnostics_by_line(
                                &diagnostics,
                                &parsed.lines,
                            ),
                            None => HashMap::new(),
                        },
                    }
                }
                _ => HashMap::new(),
            };
            self.file_view_diagnostics = diagnostics_map;

            Some(lsp_file_status(
                &state,
                companion_state.as_ref(),
                connection.as_deref(),
                file_uri.as_ref(),
            ))
        } else {
            self.file_view_diagnostics = HashMap::new();
            None
        };
        // The status bar's `N servers · M errors` reads this exact value (see
        // `AdeApp::file_view_error_count`'s own docs) instead of re-deriving a count from
        // `file_view_diagnostics`'s per-line index, so it can never disagree with the real count
        // this same frame's File view footer (`render_file_status_bar`, below) shows.
        self.file_view_error_count = match &lsp_status {
            Some(LspFileStatus::Analyzed { errors, .. }) => Some(*errors),
            _ => None,
        };

        let Some(parsed) = self.file_view_cache.as_ref() else {
            return render_sidebar_message("no file loaded".to_string(), theme::text::FAINT.into());
        };

        let cursor = self.code_cursor;
        let status_bar = render_file_status_bar(parsed, cursor, lsp_status.as_ref(), lsp_binary);
        let truncated = parsed.truncated;
        // Real, editable file-view state (Revision R8.5a): whichever `EditBuffer`
        // `spawn_file_load`'s completion already lazily seeded for `relative_path` (`None` only
        // for a truncated file, which stays read-only - see that method's own docs). Its `lines`,
        // not `parsed.lines`, is what's actually on screen from here on whenever it exists.
        // `parsed`/`file_view_cache` stays the freshness/reload source of truth (see
        // `Self::render_file_view`'s own top docs on the throttled `std::fs::metadata` check);
        // diagnostics/hover now track the *live* buffer instead, per Revision R8.5b, above.
        let line_count = self
            .edit_buffers
            .get(&relative_path_buf)
            .map(|buffer| buffer.lines.len())
            .unwrap_or_else(|| parsed.lines.len());
        // `true` while the real edit buffer for this file has genuine unsaved changes.
        //
        // Revision R8.5b: diagnostics/hover are **no longer** suppressed while dirty - now that
        // `Self::schedule_lsp_sync` keeps the server's own document state genuinely in sync with
        // live, unsaved edits (see that method's own docs, and the diagnostics-indexing fix
        // above), a diagnostic's real position is relative to the *live* buffer's own lines, the
        // exact same lines this row builder already renders from - no more confidently-wrong-row
        // risk to guard against for diagnostics specifically. `self.file_view_changed_lines`
        // (the git-gutter changed-line stripe) is a genuinely different case that still keeps
        // this same real suppression below: it comes from `wt_core::diff`, a real diff against
        // this file's content **on disk**, which has no way to know about an unsaved edit at all
        // - a line shifted by typing would still misalign that marker onto the wrong row, so
        // suppressing it while dirty is still the honest choice, not a leftover gap.
        let buffer_dirty = self
            .edit_buffers
            .get(&relative_path_buf)
            .is_some_and(|buffer| buffer.is_dirty());
        // GitHub issue #29: the current line's real, already-cached inline git blame label -
        // `None` while the buffer is dirty, while the setting is off, or while no fresh cache
        // entry exists yet for this file (see `Self::current_line_blame`'s own docs). Computed
        // once here, not per row: only the current line ever shows it.
        let inline_blame = self.inline_blame_render_model(&absolute_path, cursor, buffer_dirty, cx);
        // `true` while a dirty buffer's own content hasn't reached the language server yet, *or*
        // has reached it but the server hasn't genuinely answered for it yet (the real debounce
        // in `Self::schedule_lsp_sync` hasn't fired, there's no ready client at all, or a real
        // `didChange` was sent but its own diagnostics pull hasn't confirmed a fresh answer -
        // Revision R8.5b audit finding 6) - the one real, honest signal this phase adds in place
        // of R8.5a's old, now-inaccurate "diagnostics reflect only the last saved version"
        // banner: diagnostics *do* track live edits now, but not *instantly* - a real language
        // server's own recompute latency is real, non-zero time, and `self.file_view_diagnostics`
        // legitimately still shows whatever the server last actually reported (the previous,
        // still-real result) until a fresher one arrives, rather than flickering blank or hiding
        // the gap.
        //
        // Two real, independent gates, either of which keeps this honestly "pending":
        // - `content_unsynced`: the live buffer's content hasn't matched what was last
        //   *successfully* sent (`AdeApp::lsp_last_synced_content`) - the original, plan-time-vs-
        //   send-time signal.
        // - `diagnostics_unconfirmed`: content *was* sent (a real `AdeApp::lsp_synced_version`
        //   exists), but no confirmed diagnostics answer for that exact version has landed yet
        //   (`AdeApp::lsp_diagnostics_confirmed_version`) - the real gap between "didChange
        //   dispatched" and "a fresh diagnostics answer for that edit actually arrived" that an
        //   earlier version of this gate closed only the first half of (flipping to "synced" the
        //   instant the send succeeded, even though the server hadn't answered for it yet). A
        //   file with no `lsp_synced_version` entry at all has nothing real to be unconfirmed
        //   about yet - `content_unsynced` alone already covers that case honestly.
        let sync_pending = has_lsp
            && buffer_dirty
            && self
                .edit_buffers
                .get(&relative_path_buf)
                .is_some_and(|buffer| {
                    let content_unsynced = self.lsp_last_synced_content.get(&relative_path_buf)
                        != Some(&buffer.content);
                    let diagnostics_unconfirmed =
                        match self.lsp_synced_version.get(&relative_path_buf) {
                            Some(sent_version) => {
                                self.lsp_diagnostics_confirmed_version
                                    .get(&relative_path_buf)
                                    != Some(sent_version)
                            }
                            None => false,
                        };
                    content_unsynced || diagnostics_unconfirmed
                });
        let diagnostics_card = render_diagnostics_card(&self.file_view_diagnostics);
        // Hover only applies to a file whose extension has a real LSP identity; cloned once here
        // and reused per row for the same reason as `file_uri` above.
        let hover_target = has_lsp.then(|| absolute_path.clone());
        let row_line_height = px(self.effective_code_rem_px() * 1.6);
        let code_focus_handle = self.code_focus_handle.clone();
        let entity = cx.entity();
        let conflict = self.file_external_conflict.contains(&relative_path_buf);
        let save_error = self
            .file_save_error
            .as_ref()
            .filter(|(path, _)| path == &relative_path_buf)
            .map(|(_, message)| message.clone());
        // Captured here, before `relative_path_buf` is moved into the row-builder closure below
        // (`cx.processor`'s own `move` closure takes it by value) - the real fallback click
        // handler further down (see its own docs) needs its own independent copy of both.
        let has_buffer = self.edit_buffers.contains_key(&relative_path_buf);
        let below_content_click_path = relative_path_buf.clone();

        let mut code = uniform_list(
            "file-view-code",
            line_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, _window, cx| {
                let relative_path = relative_path_buf.clone();
                let has_buffer = this.edit_buffers.contains_key(&relative_path);
                if has_buffer {
                    let total = this
                        .edit_buffers
                        .get(&relative_path)
                        .map(|buffer| buffer.lines.len())
                        .unwrap_or(0);
                    let start = range.start.min(total);
                    let end = range.end.min(total);
                    // `AdeApp::file_view_row_layout` is transient/best-effort (see its own docs)
                    // but was never pruned per-frame, only cleared wholesale on a worktree
                    // switch - a real, measured unbounded-growth risk (one `(Bounds, ShapedLine)`
                    // retained per line ever scrolled past, for the life of the worktree
                    // session). Pruned here to just this frame's own visible range (1-based, to
                    // match the map's own key convention): any entry this drops for a row that's
                    // about to be rebuilt below is harmless - that row's own real paint, moments
                    // later this same pass, reinserts it fresh anyway.
                    let visible_line_numbers = (start + 1)..=end;
                    this.file_view_row_layout
                        .retain(|line_number, _| visible_line_numbers.contains(line_number));
                    let cursor_line = this.code_cursor;
                    let cursor_line_index = this
                        .edit_buffers
                        .get(&relative_path)
                        .map(|buffer| buffer.line_col_for_offset(buffer.cursor_offset()).0);
                    let mut rows = Vec::with_capacity(end.saturating_sub(start));
                    for index in start..end {
                        let Some(buffer) = this.edit_buffers.get(&relative_path) else {
                            break;
                        };
                        let Some(line) = buffer.lines.get(index) else {
                            break;
                        };
                        let line_number = index + 1;
                        let is_current = cursor_line == Some(line_number);
                        // Still suppressed while dirty - see `buffer_dirty`'s own docs, above,
                        // for why this one (the git-gutter changed-line stripe, sourced from a
                        // real on-disk diff) is a genuinely different case from the real-time
                        // diagnostics below, which are no longer suppressed here.
                        let is_changed =
                            !buffer_dirty && this.file_view_changed_lines.contains(&line_number);
                        let empty_diagnostics: Vec<diagnostics_view::LineDiagnostic> = Vec::new();
                        let line_diagnostics = this
                            .file_view_diagnostics
                            .get(&line_number)
                            .unwrap_or(&empty_diagnostics)
                            .clone();
                        let hovered_byte_range = this.hover.as_ref().and_then(|entry| {
                            (entry.path == absolute_path && entry.line_number == line_number)
                                .then(|| entry.byte_range.clone())
                        });
                        let selection_local = buffer.selection_within_line(index);
                        let cursor_local = buffer.cursor_within_line(index);
                        let marked_local = buffer.marked_within_line(index);
                        let context = crate::code_surface::editing::EditableLineContext {
                            entity: entity.clone(),
                            focus_handle: code_focus_handle.clone(),
                            path: relative_path.clone(),
                            line_index: index,
                            line_number,
                            line,
                            is_current,
                            is_changed,
                            is_cursor_line: cursor_line_index == Some(index),
                            selection_local,
                            cursor_local,
                            marked_local,
                            diagnostics: &line_diagnostics,
                            hovered_byte_range,
                            hover_target: hover_target.as_deref(),
                            inline_blame: is_current.then_some(inline_blame.as_ref()).flatten(),
                        };
                        rows.push(
                            crate::code_surface::editing::render_editable_file_view_line(
                                context,
                                row_line_height,
                                cx,
                            ),
                        );
                    }
                    return rows;
                }

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
                            inline_blame: is_current.then_some(inline_blame.as_ref()).flatten(),
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
        .line_height(rems(1.6))
        // Lets a real test measure this real container's own painted bounds (see
        // `editing::editing_tests::clicking_below_the_last_line_places_the_cursor_at_the_end_of_
        // the_buffer`) - a no-op outside test builds, matching every other `debug_selector` in
        // this crate.
        .debug_selector(|| "file-view-code-list".to_string());

        // Real bug fix (this revision): `uniform_list` only ever paints a real row element for a
        // real line index - the real blank space below the last rendered row (whenever the
        // file's content is shorter than the viewport) has no element at all, so a real click
        // there used to be silently swallowed. This container-level handler is the real, honest
        // fallback: it only ever fires for a click that no row's own `on_mouse_down`
        // (`render_editable_file_view_line`, which always calls `cx.stop_propagation()`) already
        // claimed, so it can never fire "underneath" an ordinary in-content click - matching
        // every real code editor's own "click past the end of the buffer places the caret at its
        // real end" behavior. Gated on a real edit buffer existing for this path (`has_buffer`):
        // the read-only fallback view (`render_file_view_line`, no buffer) has no real caret to
        // place at all.
        if has_buffer {
            let click_path = below_content_click_path;
            let click_line_count = line_count;
            code = code.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    window.focus(&this.code_focus_handle, cx);
                    // A real click moves the caret somewhere the completions popup's own anchor
                    // almost certainly no longer describes - same real dismiss-on-caret-move
                    // reasoning as the per-row click handler in `crate::code_surface::editing`.
                    this.dismiss_completions();
                    let Some(buffer) = this.edit_buffers.get_mut(&click_path) else {
                        return;
                    };
                    let end = buffer.content.len();
                    if event.modifiers.shift {
                        buffer.select_to(end);
                    } else {
                        buffer.move_to(end);
                    }
                    this.code_cursor = Some(click_line_count.max(1));
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        }

        // The real `"file-editor"` key context and `Editor*` `on_action` handlers (Revision
        // R8.5a) live on `Self::render_code_surface`'s outer, focused "code-surface" div, not
        // here - see that method's own docs for why (GPUI's real key dispatch only reaches
        // ancestors of the focused node, and this `body` is a descendant of it).
        let mut body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(render_file_breadcrumb(relative_path));

        if sync_pending {
            // Revision R8.5b's replacement for R8.5a's old (now inaccurate) "reflects only the
            // last saved version" banner - see `sync_pending`'s own docs above for the real,
            // honest condition this fires on. `debug_selector`'d (matching
            // `render_file_view_line`'s own `file-view-gutter-{n}` precedent) so a real test can
            // assert on its real, painted presence/absence rather than only on the underlying
            // boolean.
            body = body.child(
                div()
                    .debug_selector(|| "file-view-sync-pending-banner".to_string())
                    .child(render_sidebar_message(
                        "unsaved edits: syncing with the language server\u{2026} diagnostics/\
                         hover may briefly lag behind your very latest keystroke (change markers \
                         still reflect only the saved file until you save)"
                            .to_string(),
                        theme::text::FAINT.into(),
                    )),
            );
        }

        body = body.child(zoom_scoped(self.effective_code_rem_px(), code));

        if truncated {
            body = body.child(render_sidebar_message(
                "... file truncated (larger than 2 MiB) - read-only".to_string(),
                theme::text::FAINT.into(),
            ));
        }
        if conflict {
            body = body.child(render_sidebar_message(
                "external change detected: this file changed on disk while you have unsaved \
                 edits - secondary-s is blocked; press secondary-shift-s to overwrite the \
                 external change with your edits anyway"
                    .to_string(),
                theme::status::FAIL.into(),
            ));
        } else if let Some(message) = save_error {
            body = body.child(render_sidebar_message(message, theme::status::FAIL.into()));
        }
        if let Some(card) = diagnostics_card {
            body = body.child(card);
        }
        // Real Hover popover (this revision): no longer embedded here as an in-flow child - see
        // `Self::render_hover_card`'s own docs for why it now paints as a real, absolutely-
        // positioned top-level sibling instead (`crate::root::AdeApp::render`), the same real
        // mechanism `Self::render_completions_popover` already established.

        body.child(status_bar).into_any_element()
    }
}

/// The File view's breadcrumb, built from `relative_path`'s segments
/// (`code_view::breadcrumb_segments`). The design's deeper symbol-path suffix (`› impl
/// QueryBuilder › build`) is out of scope: it needs symbol/AST-position tracking this read-only
/// viewer doesn't build. The last (file name) segment is the active crumb; earlier segments are
/// dimmer ancestor directories.
pub(in crate::code_surface) fn render_file_breadcrumb(relative_path: &Path) -> impl IntoElement {
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

/// Bundles [`render_file_view_line`]'s two hover-state parameters to keep that function's
/// argument count under clippy's `too_many_arguments` limit; not otherwise a conceptual unit.
pub(in crate::code_surface) struct HoverRenderContext<'a> {
    /// The current file's absolute path, `Some` only for a `.rs` file.
    target: Option<&'a Path>,
    /// [`AdeApp::hover`]'s current entry, if any.
    entry: Option<&'a HoverEntry>,
    /// GitHub issue #29's real, already-computed inline blame label for *this* line - `Some`
    /// only on whichever line is `is_current`; every other row is always `None` (only the
    /// current line ever shows it). Bundled in here rather than as a ninth positional parameter
    /// on [`render_file_view_line`], for the same `too_many_arguments` reason this struct
    /// already exists.
    inline_blame: Option<&'a blame::InlineBlameLabel>,
}

pub(in crate::code_surface) fn render_file_view_line(
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
        inline_blame,
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
                .unwrap_or(theme::syntax::ERROR_UNDERLINE.into());
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
    // GitHub issue #29: the current line's real inline git blame, dimmed, at the end of the
    // row - `inline_blame` is only ever `Some` on the current line (see
    // `HoverRenderContext::inline_blame`'s own docs).
    if let Some(label) = inline_blame {
        text_row = text_row.child(blame_view::render_inline_blame_span(label, line_number));
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
                    theme::ColorToken(work_surface::TRANSPARENT)
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

/// The dot color and text the File view's footer shows for one real [`LspFileStatus`], given the
/// real *primary* server binary backing this file (`crate::language::lsp_binary_for_extension`'s
/// own answer for its extension - `None` only when the caller couldn't resolve one, which for a
/// file that has any status at all shouldn't happen, and falls back to the honest generic word
/// "language server" rather than naming some other language's binary).
///
/// Derived, never hardcoded: these strings used to say `"rust-analyzer"` literally, for every
/// language. That was merely generic until a two-server language could produce a status of its
/// own. `LspConnection::liveness_failure_reason` names the real dead process, so a dead Vue
/// companion rendered as the actively-wrong `"rust-analyzer: typescript-language-server (vue)'s
/// connection was lost..."`, and a `.vue` file mid-spawn said `"starting rust-analyzer..."` while
/// `vue-language-server` was the thing actually starting.
///
/// [`LspFileStatus::Failed`]'s own message deliberately gets **no** prefix at all: every real
/// source of it already names its own server (`lsp_core::LspError`'s variants all carry `server`,
/// `liveness_failure_reason` carries `LspClient::name()`, and the companion's prerequisite errors
/// name Vue), so prefixing would either duplicate that name or contradict it.
fn lsp_status_label(status: &LspFileStatus, binary: Option<&str>) -> (gpui::Rgba, String) {
    let binary = binary.unwrap_or("language server");
    match status {
        LspFileStatus::Spawning => (theme::text::GHOST.into(), format!("starting {binary}...")),
        LspFileStatus::Failed(message) => (theme::status::FAIL.into(), message.clone()),
        LspFileStatus::Indexing => (theme::status::ASK.into(), format!("{binary}: indexing...")),
        LspFileStatus::Analyzed { errors, warnings } => {
            let color = if *errors > 0 {
                theme::status::FAIL
            } else {
                theme::status::REVIEW
            };
            let label = if *errors == 0 && *warnings == 0 {
                format!("{binary}: no diagnostics")
            } else {
                format!("{binary}: {errors} errors, {warnings} warnings")
            };
            (color.into(), label)
        }
    }
}

/// The File view's status bar: language, last-click cursor line (`None` until the first click,
/// per `AdeApp::code_cursor`), a byte-detected line-ending label, and - for a file with a live LSP
/// client - that language's own real server status (see [`lsp_status_label`]). The design's
/// `col 14` is deliberately omitted: there's no per-character column tracking in this app, so
/// showing a column would always read `1`.
pub(in crate::code_surface) fn render_file_status_bar(
    parsed: &code_view::ParsedFile,
    cursor: Option<usize>,
    lsp_status: Option<&LspFileStatus>,
    lsp_binary: Option<&str>,
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
        let (dot_color, label) = lsp_status_label(status, lsp_binary);
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

/// Regression coverage for Revision R8.5b's `AdeApp::sync_pending` gate (the replacement for
/// R8.5a's old, now-inaccurate "diagnostics reflect only the last saved version" banner - see
/// `AdeApp::render_file_view`'s own `sync_pending` docs). Confirms the real banner is absent on a
/// clean buffer (the legitimate case must not regress), present the instant a buffer becomes
/// dirty with no real sync recorded yet, and disappears again - the real, new behavior this fix
/// specifically adds - once `AdeApp::lsp_last_synced_content` shows the language server has
/// genuinely been told about this exact content, even though the buffer is still dirty (unsaved).
/// `lsp_last_synced_content` is seeded directly rather than waiting on a real spawned
/// rust-analyzer, matching `crate::lsp::client::lsp_client_eviction_tests`' own established
/// precedent for testing real bookkeeping without paying for a real process spawn - the full,
/// real end-to-end proof (a genuine `rust-analyzer`/`typescript-language-server`/
/// `pyright-langserver` round trip) lives in `crate::lsp::client::lsp_diagnostics_wiring_tests`.
#[cfg(test)]
mod dirty_buffer_stale_decoration_tests {
    use super::*;
    use gpui::TestAppContext;

    const BANNER_SELECTOR: &str = "file-view-sync-pending-banner";

    #[gpui::test]
    fn the_honest_sync_pending_banner_tracks_real_sync_state_not_just_raw_dirtiness(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {\n    1\n}\n").expect("write sample.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_none(),
            "the legitimate, clean-buffer case must not regress: a freshly-opened, unedited \
             buffer must never show the sync-pending banner"
        );

        // Dirty the buffer with a real edit - no real sync has been recorded for it yet.
        let relative = PathBuf::from("sample.rs");
        app.update(cx, |app, cx| {
            app.edit_buffers
                .get_mut(&relative)
                .expect("real edit buffer should have been seeded for sample.rs")
                .replace_range(None, "// ");
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_some(),
            "a genuinely dirty buffer with no real sync recorded yet must show the honest \
             sync-pending banner"
        );

        // The real debounce settles and `Self::schedule_lsp_sync`'s async continuation records
        // this exact content as synced (`AdeApp::lsp_last_synced_content`) - the banner must
        // disappear even though the buffer is still, correctly, dirty (unsaved). Seeding only
        // `lsp_last_synced_content` here (not `lsp_synced_version`/
        // `lsp_diagnostics_confirmed_version`) deliberately exercises the plain content-match
        // half of `sync_pending`'s real gate in isolation - the version-confirmation half
        // (Revision R8.5b audit finding 6) has its own dedicated coverage in
        // `sync_pending_diagnostics_confirmation_tests`, below.
        let synced_content = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .expect("buffer")
                .content
                .clone()
        });
        app.update(cx, |app, cx| {
            app.lsp_last_synced_content
                .insert(relative.clone(), synced_content);
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_none(),
            "once the language server has genuinely been told about this exact content, the \
             banner must disappear - the buffer being dirty (unsaved) alone is not the real \
             condition this banner tracks"
        );
        assert!(
            app.read_with(cx, |app, _| app
                .edit_buffers
                .get(&relative)
                .expect("buffer")
                .is_dirty()),
            "sanity check: the buffer must still genuinely be dirty/unsaved at this point"
        );

        // A further real edit moves the content past what was last synced - the banner must
        // reappear.
        app.update(cx, |app, cx| {
            app.edit_buffers
                .get_mut(&relative)
                .expect("buffer")
                .replace_range(None, "more ");
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_some(),
            "a further real edit past what was last synced must bring the honest banner back"
        );
    }
}

/// Revision R8.5b audit finding 6's direct regression coverage: the real `sync_pending` banner
/// must stay honestly "pending" for the whole real gap between "content was successfully sent"
/// and "a version-matched diagnostics answer for it actually landed" - not flip to "synced" the
/// instant the send alone succeeds. `AdeApp::lsp_last_synced_content`/`AdeApp::lsp_synced_version`/
/// `AdeApp::lsp_diagnostics_confirmed_version` are seeded directly (matching
/// `dirty_buffer_stale_decoration_tests`' own established precedent for testing this real gate
/// without a real LSP round trip) rather than waiting on one.
#[cfg(test)]
mod sync_pending_diagnostics_confirmation_tests {
    use super::*;
    use gpui::TestAppContext;

    const BANNER_SELECTOR: &str = "file-view-sync-pending-banner";

    #[gpui::test]
    fn the_banner_stays_pending_until_a_version_matched_diagnostics_answer_lands(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.rs");
        std::fs::write(&file_path, "fn main() {\n    1\n}\n").expect("write sample.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();

        let relative = PathBuf::from("sample.rs");
        app.update(cx, |app, cx| {
            app.edit_buffers
                .get_mut(&relative)
                .expect("real edit buffer should have been seeded for sample.rs")
                .replace_range(None, "// ");
            cx.notify();
        });

        // The content was genuinely sent (`lsp_last_synced_content`/`lsp_synced_version` both
        // recorded, matching a real, successful `did_change_full` - see `Self::schedule_lsp_sync`'s
        // own docs for exactly where this write happens now, post-success), but *no* confirmed
        // diagnostics answer for that version has landed yet - the real, honest "sent but not yet
        // answered" gap this fix exists to keep the banner truthful through.
        let synced_content = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .expect("buffer")
                .content
                .clone()
        });
        app.update(cx, |app, cx| {
            app.lsp_last_synced_content
                .insert(relative.clone(), synced_content);
            app.lsp_synced_version.insert(relative.clone(), 7);
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_some(),
            "content sent (version 7) but no confirmed diagnostics answer for that version yet \
             must keep the honest sync-pending banner showing - the send alone is not \
             confirmation"
        );

        // A confirmed answer for an *older* version (a real, late-arriving stale confirmation -
        // see finding 5's own version-guard) must not satisfy this either.
        app.update(cx, |app, cx| {
            app.lsp_diagnostics_confirmed_version
                .insert(relative.clone(), 6);
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_some(),
            "a confirmed answer for an older version (6) than what was actually sent (7) must \
             not clear the honest sync-pending banner"
        );

        // The real, version-matched confirmation lands - only now should the banner clear.
        app.update(cx, |app, cx| {
            app.lsp_diagnostics_confirmed_version
                .insert(relative.clone(), 7);
            cx.notify();
        });
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        assert!(
            cx.debug_bounds(BANNER_SELECTOR).is_none(),
            "once a real, version-matched diagnostics answer has genuinely landed, the honest \
             sync-pending banner must clear"
        );
    }
}

/// Coverage for the File view footer's real server label (see [`lsp_status_label`]) - Revision R11
/// audit finding 2: every one of these strings was hardcoded to `"rust-analyzer"` regardless of
/// which language was actually open.
#[cfg(test)]
mod lsp_status_label_tests {
    use super::*;

    /// The real binary names this app's own registry resolves, used exactly as
    /// `AdeApp::render_file_view` passes them in - not string literals invented here, so a
    /// registry rename can't leave this test passing against a name nothing uses.
    fn binary_for(extension: &str) -> Option<&'static str> {
        language::lsp_binary_for_extension(Some(extension))
    }

    #[test]
    fn the_server_name_in_every_label_is_derived_from_the_real_language() {
        assert_eq!(binary_for("rs"), Some("rust-analyzer"));
        assert_eq!(binary_for("vue"), Some("vue-language-server"));

        assert_eq!(
            lsp_status_label(&LspFileStatus::Spawning, binary_for("rs")).1,
            "starting rust-analyzer..."
        );
        assert_eq!(
            lsp_status_label(&LspFileStatus::Spawning, binary_for("vue")).1,
            "starting vue-language-server...",
            "a .vue file is waiting on vue-language-server, not on rust-analyzer"
        );
        assert_eq!(
            lsp_status_label(&LspFileStatus::Indexing, binary_for("py")).1,
            "pyright-langserver: indexing..."
        );
        assert_eq!(
            lsp_status_label(
                &LspFileStatus::Analyzed {
                    errors: 0,
                    warnings: 0
                },
                binary_for("ts")
            )
            .1,
            "typescript-language-server: no diagnostics"
        );
        assert_eq!(
            lsp_status_label(
                &LspFileStatus::Analyzed {
                    errors: 2,
                    warnings: 1
                },
                binary_for("vue")
            )
            .1,
            "vue-language-server: 2 errors, 1 warnings"
        );
    }

    /// The specific bug this issue's own new `LspConnection::liveness_failure_reason` made
    /// actively wrong rather than merely generic: it already names the real dead process, so the
    /// old `format!("rust-analyzer: {message}")` produced a label naming two different servers,
    /// one of which had nothing to do with the file.
    #[test]
    fn a_failure_message_is_shown_as_is_because_it_already_names_its_own_server() {
        let dead_companion =
            "typescript-language-server (vue)'s connection was lost (the process exited \
             unexpectedly)"
                .to_string();
        let (_, label) = lsp_status_label(
            &LspFileStatus::Failed(dead_companion.clone()),
            binary_for("vue"),
        );
        assert_eq!(label, dead_companion);
        assert!(
            !label.contains("rust-analyzer"),
            "a dead Vue companion must never be reported under rust-analyzer's name, got: {label}"
        );

        // The single-server path's own message is equally self-describing (every
        // `lsp_core::LspError` variant carries its own `server`), so it is also shown untouched.
        let dead_primary = "failed to spawn `rust-analyzer` (is it installed and on PATH?)";
        assert_eq!(
            lsp_status_label(
                &LspFileStatus::Failed(dead_primary.to_string()),
                binary_for("rs")
            )
            .1,
            dead_primary
        );
    }

    /// The honest fallback when no binary could be resolved at all - a generic word, never some
    /// other language's real server name.
    #[test]
    fn an_unresolved_binary_falls_back_to_a_generic_word_not_another_language() {
        assert_eq!(
            lsp_status_label(&LspFileStatus::Spawning, None).1,
            "starting language server..."
        );
        assert_eq!(
            lsp_status_label(&LspFileStatus::Indexing, None).1,
            "language server: indexing..."
        );
    }
}
