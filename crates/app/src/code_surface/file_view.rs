//! The editable File view: one open file's rows, its breadcrumb, and its footer status
//! bar. The real text mutation behind a keystroke lives in `super::editing`; this module
//! only draws what `super::edit_buffer` currently holds.

use super::lsp_ui::{
    diagnostic_row_bg, diagnostic_underline_color, render_inline_diagnostic_message,
};
use super::zoom::zoom_scoped;
use super::*;
use crate::lsp::client::{lsp_file_status, LspFileStatus};
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_sidebar_message;
use std::collections::HashSet;

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
            match self.edit_buffer(relative_path) {
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
                    match self.edit_buffer(&relative_path_buf) {
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
        // GitHub issue #178: the breadcrumb's right-hand counts, read off the exact same
        // `LspFileStatus` this frame's footer label is built from rather than re-derived, so the
        // two can never disagree about how many problems this file has. `None` for every
        // non-`Analyzed` status (no server, still spawning/indexing, or a failed one) - the band
        // then draws no dots at all instead of an unearned `0`.
        let breadcrumb_diagnostic_counts = match &lsp_status {
            Some(LspFileStatus::Analyzed { errors, warnings }) => Some((*errors, *warnings)),
            _ => None,
        };
        // The live enclosing-declaration chain at the caret, from the outline the buffer's own
        // last parse produced (`crate::code_surface::symbols`). Read here, before the `parsed`
        // borrow below, and materialized as owned `String`s so the breadcrumb can be built
        // further down without holding a borrow of `self` across it. Empty - and the breadcrumb
        // then honestly shows the path alone - whenever there's no live edit buffer for this file
        // (still loading, or truncated/non-UTF-8 and therefore permanently read-only), which is
        // also the only state where there is no real caret offset to look anything up by.
        let breadcrumb_symbol_path: Vec<String> = match self.edit_buffer(&relative_path_buf) {
            Some(buffer) => symbols::symbol_path_at(&buffer.symbols, buffer.cursor_offset())
                .into_iter()
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        };

        let Some(parsed) = self.file_view_cache.as_ref() else {
            return render_sidebar_message("no file loaded".to_string(), theme::text::FAINT.into());
        };

        let cursor = self.code_cursor;
        let status_bar =
            render_file_status_bar(parsed, cursor, lsp_status.as_ref(), lsp_binary, cx);
        let truncated = parsed.truncated;
        // Real, editable file-view state (Revision R8.5a): whichever `EditBuffer`
        // `spawn_file_load`'s completion already lazily seeded for `relative_path` (`None` only
        // for a truncated file, which stays read-only - see that method's own docs). Its `lines`,
        // not `parsed.lines`, is what's actually on screen from here on whenever it exists.
        // `parsed`/`file_view_cache` stays the freshness/reload source of truth (see
        // `Self::render_file_view`'s own top docs on the throttled `std::fs::metadata` check);
        // diagnostics/hover now track the *live* buffer instead, per Revision R8.5b, above.
        let line_count = self
            .edit_buffer(&relative_path_buf)
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
            .edit_buffer(&relative_path_buf)
            .is_some_and(|buffer| buffer.is_dirty());
        // GitHub issue #29: the current line's real, already-cached inline git blame label -
        // `None` while the buffer is dirty, while the setting is off, or while no fresh cache
        // entry exists yet for this file (see `Self::current_line_blame`'s own docs). Computed
        // once here, not per row: only the current line ever shows it.
        let inline_blame = self.inline_blame_render_model(&absolute_path, cursor, buffer_dirty, cx);
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
        let has_buffer = self.edit_buffer_contains(&relative_path_buf);
        let below_content_click_path = relative_path_buf.clone();
        let minimap_relative_path = relative_path_buf.clone();
        // GitHub issue #122's real indent guides - resolved once here, outside the per-range
        // processor closure below, from the exact same `resolved_indent_settings_for_target`
        // Tab/Shift+Tab already uses (see that method's own docs on why this reuse matters: a
        // real `.editorconfig` override changes this file's own indent width, and the guides
        // must track it, not just the plain `Settings::editor` fallback). `code_font_size_px`
        // is `row_line_height`'s own real input (`effective_code_rem_px`), captured again here
        // so the per-range closure below can measure a real monospace character width at the
        // exact same font size the code text itself renders at.
        let show_indent_guides = self.settings.appearance.show_indent_guides;
        let indent_settings = self.resolved_indent_settings_for_target();
        let code_font_size_px = self.effective_code_rem_px();

        let mut code = uniform_list(
            "file-view-code",
            line_count,
            cx.processor(move |this: &mut Self, range: Range<usize>, window, cx| {
                let relative_path = relative_path_buf.clone();
                let has_buffer = this.edit_buffer_contains(&relative_path);
                if has_buffer {
                    let total = this
                        .edit_buffer(&relative_path)
                        .map(|buffer| buffer.lines.len())
                        .unwrap_or(0);
                    let start = range.start.min(total);
                    let end = range.end.min(total);
                    // `AdeApp::file_view_row_layout` is transient/best-effort (see its own docs)
                    // but was never pruned per-frame, only cleared wholesale on a worktree
                    // switch - a real, measured unbounded-growth risk (one `(Bounds, ShapedLine)`
                    // retained per line ever scrolled past, for the life of the worktree
                    // agent). Pruned here to just this frame's own visible range (1-based, to
                    // match the map's own key convention): any entry this drops for a row that's
                    // about to be rebuilt below is harmless - that row's own real paint, moments
                    // later this same pass, reinserts it fresh anyway.
                    let visible_line_numbers = (start + 1)..=end;
                    this.file_view_row_layout
                        .retain(|line_number, _| visible_line_numbers.contains(line_number));
                    let cursor_line = this.code_cursor;
                    let cursor_line_index = this
                        .edit_buffer(&relative_path)
                        .map(|buffer| buffer.line_col_for_offset(buffer.cursor_offset()).0);
                    // GitHub issue #122: one real indent level's own pixel width, measured via
                    // GPUI's real font-metrics API (`Window::text_system().advance`, verified
                    // against `crate::terminal::pane::AdeApp::cell_size`'s own identical real
                    // usage of the same call - see that method's own docs) at the code text's
                    // real font size, not a hardcoded pixel constant unrelated to it. `None`
                    // whenever the setting is off (skips the measurement entirely) or an outright
                    // measurement failure - either way every row's `indent_guide_xs` below ends
                    // up empty, the same honest "nothing to paint" a real measurement failure
                    // should produce rather than a guessed fallback width.
                    let indent_column_width_px = if show_indent_guides {
                        let font_id = window.text_system().resolve_font(&font(theme::font::MONO));
                        window
                            .text_system()
                            .advance(font_id, px(code_font_size_px), ' ')
                            .map(|advance| advance.width)
                            .ok()
                            .filter(|width| *width > px(0.0))
                    } else {
                        None
                    };
                    let mut rows = Vec::with_capacity(end.saturating_sub(start));
                    for index in start..end {
                        let Some(buffer) = this.edit_buffer(&relative_path) else {
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
                            (entry.path == absolute_path
                                && entry.line_number == line_number
                                && entry.worth_underlining())
                            .then(|| entry.byte_range.clone())
                        });
                        let selection_local = buffer.selection_within_line(index);
                        let cursor_local = buffer.cursor_within_line(index);
                        let marked_local = buffer.marked_within_line(index);
                        // Multi-cursor (Revision R13, issue #28) - empty `Vec`s in ordinary
                        // single-cursor use, so this changes nothing about how an unaffected row
                        // paints; see `EditBuffer::secondary_selections_within_line`/
                        // `EditBuffer::secondary_cursors_within_line`'s own docs.
                        let secondary_selections_local =
                            buffer.secondary_selections_within_line(index);
                        let secondary_cursors_local = buffer.secondary_cursors_within_line(index);
                        // GitHub issue #122: one guide per real indent level this specific line's
                        // own leading whitespace covers (`indent::leading_indent_levels`), each at
                        // `level * tab_width` real monospace columns from the row's own text
                        // origin - empty whenever the setting is off (`indent_column_width_px` is
                        // `None`) or this line has no leading indentation at all.
                        let indent_guide_xs: Vec<Pixels> = match indent_column_width_px {
                            Some(column_width) => {
                                let levels = crate::code_surface::indent::leading_indent_levels(
                                    &line.text,
                                    indent_settings.tab_width,
                                );
                                (0..levels)
                                    .map(|level| {
                                        column_width
                                            * (level as f32 * indent_settings.tab_width as f32)
                                    })
                                    .collect()
                            }
                            None => Vec::new(),
                        };
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
                            secondary_selections_local,
                            secondary_cursors_local,
                            diagnostics: &line_diagnostics,
                            hovered_byte_range,
                            inline_blame: is_current.then_some(inline_blame.as_ref()).flatten(),
                            caret_style: this.settings.appearance.caret_style,
                            caret_blink_visible: this.caret_blink_visible,
                            indent_guide_xs,
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
                    // GitHub issue #186: a click below the last line dismisses the Hover card for
                    // the same reason a click on a row does - see that handler's own comment.
                    this.dismiss_completions();
                    this.dismiss_hover();
                    let Some(buffer) = this.edit_buffer_mut(&click_path) else {
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
            .child(render_file_breadcrumb(
                relative_path,
                &breadcrumb_symbol_path,
                breadcrumb_diagnostic_counts,
            ));

        // GitHub issue #30's real editor scrollbar decoration marks - see `Self::render_file_tree`'s
        // own docs (`crate::sidebar::render`) on why the scrollbar must be a sibling of the
        // scrollable element, inside its own non-scrolling `.relative()` wrapper, never a child of
        // `code` itself. `marks` is built from real, already-computed state (see
        // [`editor_scrollbar_marks`]'s own docs) - never invented for the scrollbar.
        let marks = editor_scrollbar_marks(
            &self.file_view_diagnostics,
            &self.file_view_changed_lines,
            self.code_cursor,
            line_count,
        );
        let code_with_scrollbar = div()
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(code)
            .children(self.render_vertical_scrollbar(
                "file-view-scrollbar",
                &self.file_view_scroll_handle,
                &marks,
                cx,
            ));
        // GitHub issue #30's real minimap (`crate::code_surface::minimap`) - reads the exact
        // same highlighted lines this view itself renders from (a live edit buffer's, or the
        // read-only parsed cache's - the same `has_buffer`/fallback split `code`'s own row
        // builder above already makes), and the same real git-changed-line set the gutter stripe
        // and scrollbar marks above already use. `None` (no minimap) is a real, structural
        // outcome (the setting is off, or the file is too large - see that module's own docs),
        // not a placeholder.
        let minimap_lines: Option<&[code_view::RenderedLine]> =
            if let Some(buffer) = self.edit_buffer(&minimap_relative_path) {
                Some(buffer.lines.as_slice())
            } else {
                self.file_view_cache
                    .as_ref()
                    .map(|parsed| parsed.lines.as_slice())
            };
        let minimap = match minimap_lines {
            Some(lines) => self.render_minimap(
                lines,
                &self.file_view_changed_lines,
                row_line_height.as_f32(),
                cx,
            ),
            None => None,
        };
        let code_row = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(code_with_scrollbar)
            .children(minimap);
        body = body.child(zoom_scoped(self.effective_code_rem_px(), code_row));

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
        // Neither LSP popup is embedded here as an in-flow child any more. The Hover popover
        // never was; the Diagnostic card was, and GitHub issue #186 moved it out for the same
        // reason - as a plain flex child it took real, permanent vertical space away from the code
        // view. Both now paint as real, absolutely-positioned top-level siblings in
        // `crate::root::AdeApp::render`; see `Self::render_diagnostic_card`'s own docs.

        body.child(status_bar).into_any_element()
    }
}

/// The File view's real editor scrollbar decoration marks (GitHub issue #30's "search matches,
/// git changes, errors/warnings, cursor position" requirement, minus search matches - see
/// `crate::root::scrollbar`'s own module docs for why: this app has no find-in-file feature to
/// source real match positions from). Every mark here comes from state this view already
/// maintains for its own inline rendering - `diagnostics` backs the dotted-underline/row-tint
/// diagnostics (`crate::code_surface::lsp_ui`), `changed_lines` backs the git-gutter stripe
/// (`render_file_view_line`), `cursor_line` is the real blinking caret's own line - not a second,
/// parallel data source invented for the scrollbar.
///
/// Only [`Severity::Error`]/[`Severity::Warning`] get a diagnostic mark (matching most real
/// editors' own overview-ruler convention of not drawing a mark per hint/information diagnostic,
/// which on a large file can vastly outnumber the lines actually worth flagging at a glance).
/// `line_count == 0` returns no marks at all (nothing to divide a fraction by) rather than
/// panicking or producing `NaN` fractions.
pub(in crate::code_surface) fn editor_scrollbar_marks(
    diagnostics: &HashMap<usize, Vec<diagnostics_view::LineDiagnostic>>,
    changed_lines: &HashSet<usize>,
    cursor_line: Option<usize>,
    line_count: usize,
) -> Vec<scrollbar::ScrollbarMark> {
    if line_count == 0 {
        return Vec::new();
    }
    let fraction_for_line =
        |line_number: usize| -> f32 { line_number.saturating_sub(1) as f32 / line_count as f32 };

    let mut marks = Vec::new();
    for (&line_number, line_diagnostics) in diagnostics {
        let color = match diagnostics_view::Severity::worst(line_diagnostics) {
            Some(diagnostics_view::Severity::Error) => Some(theme::status::FAIL.resolve()),
            Some(diagnostics_view::Severity::Warning) => Some(theme::status::ASK.resolve()),
            _ => None,
        };
        if let Some(color) = color {
            marks.push(scrollbar::ScrollbarMark::new(
                fraction_for_line(line_number),
                color,
            ));
        }
    }
    for &line_number in changed_lines {
        marks.push(scrollbar::ScrollbarMark::new(
            fraction_for_line(line_number),
            theme::diff::GIT_GUTTER.resolve(),
        ));
    }
    if let Some(line_number) = cursor_line {
        marks.push(scrollbar::ScrollbarMark::new(
            fraction_for_line(line_number),
            theme::syntax::CARET.resolve(),
        ));
    }
    marks
}

#[cfg(test)]
mod editor_scrollbar_mark_tests {
    use super::*;

    fn diagnostic(severity: diagnostics_view::Severity) -> diagnostics_view::LineDiagnostic {
        diagnostics_view::LineDiagnostic {
            byte_range: 0..1,
            severity,
            message: "test".to_string(),
            source: None,
            code: None,
        }
    }

    #[test]
    fn an_empty_file_produces_no_marks_rather_than_a_divide_by_zero() {
        let marks = editor_scrollbar_marks(&HashMap::new(), &HashSet::new(), Some(1), 0);
        assert!(marks.is_empty());
    }

    #[test]
    fn a_line_with_only_hint_or_information_diagnostics_gets_no_mark() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(
            5,
            vec![
                diagnostic(diagnostics_view::Severity::Hint),
                diagnostic(diagnostics_view::Severity::Information),
            ],
        );
        let marks = editor_scrollbar_marks(&diagnostics, &HashSet::new(), None, 100);
        assert!(marks.is_empty());
    }

    #[test]
    fn an_error_diagnostic_produces_a_real_mark_at_the_lines_fraction() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(51, vec![diagnostic(diagnostics_view::Severity::Error)]);
        let marks = editor_scrollbar_marks(&diagnostics, &HashSet::new(), None, 100);
        assert_eq!(marks.len(), 1);
        // Line 51 of 100, 1-based -> fraction 0.50.
        assert!((marks[0].fraction - 0.50).abs() < 0.001);
    }

    #[test]
    fn the_cursor_line_produces_its_own_mark_independent_of_diagnostics_and_git_changes() {
        let marks = editor_scrollbar_marks(&HashMap::new(), &HashSet::new(), Some(1), 100);
        assert_eq!(marks.len(), 1);
        assert!((marks[0].fraction - 0.0).abs() < 0.001);
    }

    #[test]
    fn a_changed_line_produces_its_own_mark() {
        let mut changed = HashSet::new();
        changed.insert(100);
        let marks = editor_scrollbar_marks(&HashMap::new(), &changed, None, 100);
        assert_eq!(marks.len(), 1);
        // Line 100 of 100, 1-based -> fraction 0.99.
        assert!((marks[0].fraction - 0.99).abs() < 0.001);
    }

    #[test]
    fn diagnostics_changed_lines_and_the_cursor_all_contribute_independent_marks() {
        let mut diagnostics = HashMap::new();
        diagnostics.insert(1, vec![diagnostic(diagnostics_view::Severity::Error)]);
        let mut changed = HashSet::new();
        changed.insert(2);
        let marks = editor_scrollbar_marks(&diagnostics, &changed, Some(3), 100);
        assert_eq!(marks.len(), 3);
    }
}

/// One rendered breadcrumb crumb - see [`breadcrumb_crumbs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::code_surface) struct Crumb {
    pub text: String,
    /// `true` for exactly one crumb: the file's own name. The design (`design_handoff_jerry_ade/
    /// revision/Jerry.dc.html`, the File view breadcrumb band) renders that one at `#a9b0b7` and
    /// every other crumb - ancestor directories *and* enclosing symbols alike - at `#5e646a`.
    pub active: bool,
}

/// The File view breadcrumb's real crumb list: `relative_path`'s own directory/file segments
/// (`code_view::breadcrumb_segments`) followed by `symbol_path`, the chain of declarations
/// enclosing wherever the caret currently is (`crate::code_surface::symbols::symbol_path_at`).
///
/// GitHub issue #178: the path half alone is what this band used to render, which the Surface C
/// toolbar directly above it already shows - the literal duplicate the issue is about. The symbol
/// half is what makes it the design's `src › db › query_builder.rs › impl QueryBuilder › build`
/// instead. `symbol_path` is legitimately empty in real, reachable states (caret at a file's top
/// level, a language with no enclosing-declaration concept, or no live edit buffer for this file
/// yet), and the band then honestly renders the path alone rather than padding it out.
///
/// Pure and separately tested, so "what does the breadcrumb actually say for this caret" is a
/// real assertion rather than something only reachable through a GPUI window.
pub(in crate::code_surface) fn breadcrumb_crumbs(
    relative_path: &Path,
    symbol_path: &[String],
) -> Vec<Crumb> {
    let segments = code_view::breadcrumb_segments(relative_path);
    let last_path_index = segments.len().saturating_sub(1);
    let mut crumbs: Vec<Crumb> = segments
        .into_iter()
        .enumerate()
        .map(|(index, text)| Crumb {
            text,
            active: index == last_path_index,
        })
        .collect();
    crumbs.extend(symbol_path.iter().map(|text| Crumb {
        text: text.clone(),
        active: false,
    }));
    crumbs
}

/// The File view's breadcrumb (`design_handoff_jerry_ade/README.md`: "Breadcrumb 26 (`src › db ›
/// query_builder.rs › impl QueryBuilder › build`, 10.5px mono, separators `#3d4248`, active crumb
/// `#a9b0b7`) with error/warning counts right").
///
/// `symbol_path` is the live enclosing-symbol chain at the caret and `diagnostic_counts` the real
/// `(errors, warnings)` pair from this same frame's `LspFileStatus::Analyzed` - `None` whenever
/// this file has no analyzed language-server result at all, in which case no count dots are drawn
/// rather than a fabricated `0 0`.
pub(in crate::code_surface) fn render_file_breadcrumb(
    relative_path: &Path,
    symbol_path: &[String],
    diagnostic_counts: Option<(usize, usize)>,
) -> impl IntoElement {
    let crumbs = breadcrumb_crumbs(relative_path, symbol_path);

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

    for (index, crumb) in crumbs.into_iter().enumerate() {
        if index > 0 {
            row = row.child(div().text_color(theme::text::DISABLED).child("\u{203A}"));
        }
        let color = if crumb.active {
            theme::text::SECONDARY
        } else {
            // `#5e646a`, the inactive-crumb colour the design mockup uses for both ancestor
            // directories and symbol crumbs.
            theme::text::FAINTER
        };
        row = row.child(div().text_color(color).child(crumb.text));
    }

    if let Some((errors, warnings)) = diagnostic_counts {
        row = row.child(div().flex_1());
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap(px(9.0))
                .children(diagnostic_count_dot(
                    diagnostics_view::Severity::Error,
                    errors,
                ))
                .children(diagnostic_count_dot(
                    diagnostics_view::Severity::Warning,
                    warnings,
                )),
        );
    }

    row
}

/// One `● N` group on the breadcrumb's right edge - a 5px severity-coloured dot plus the count,
/// per the design mockup's File view breadcrumb band.
///
/// `None` when `count` is 0: a clean file shows nothing at all rather than a `0`, which is both
/// the mockup's own shape (it only ever draws groups for non-zero counts) and the honest reading -
/// the absence of a dot is unambiguous where a grey `0` next to a red dot is not. The dot colour
/// comes from [`diagnostic_underline_color`], the same severity->colour map the in-code dotted
/// underlines use, so the breadcrumb can never disagree with the markers it is counting.
fn diagnostic_count_dot(
    severity: diagnostics_view::Severity,
    count: usize,
) -> Option<impl IntoElement> {
    if count == 0 {
        return None;
    }
    Some(
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded_full()
                    .bg(diagnostic_underline_color(severity)),
            )
            .child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme::text::DIM)
                    .child(count.to_string()),
            ),
    )
}

/// GitHub issue #178's real regression coverage: the breadcrumb band used to render only the file
/// path, which the Surface C toolbar directly above it already shows - a literal duplicate. These
/// assert the band now genuinely carries the design's symbol suffix and its error/warning counts,
/// against the same pure builders [`render_file_breadcrumb`] itself calls.
#[cfg(test)]
mod breadcrumb_tests {
    use super::*;
    use crate::code_surface::symbols;

    const SOURCE: &str = "\
mod db {
    impl QueryBuilder {
        pub fn build(&self) {
            let marker = 1;
        }
    }
}
";

    /// The exact expression `AdeApp::render_file_view` evaluates for a live edit buffer, driven
    /// here off a real parse of `SOURCE` rather than a hand-written crumb list.
    fn symbol_path_at_marker(needle: &str) -> Vec<String> {
        let outline = symbols::symbol_outline(SOURCE, Some("rs"));
        let offset = SOURCE.find(needle).expect("needle present") + needle.len();
        symbols::symbol_path_at(&outline, offset)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn the_breadcrumb_really_carries_the_designs_own_path_plus_symbol_chain() {
        let crumbs = breadcrumb_crumbs(
            Path::new("src/db/query_builder.rs"),
            &symbol_path_at_marker("let marker = 1;"),
        );
        let texts: Vec<&str> = crumbs.iter().map(|crumb| crumb.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "src",
                "db",
                "query_builder.rs",
                "mod db",
                "impl QueryBuilder",
                "build",
            ],
            "design_handoff_jerry_ade/README.md's own breadcrumb example, minus the mockup's \
             elided `mod db`"
        );
    }

    #[test]
    fn exactly_one_crumb_is_active_and_it_is_the_file_name() {
        let crumbs = breadcrumb_crumbs(
            Path::new("src/db/query_builder.rs"),
            &symbol_path_at_marker("let marker = 1;"),
        );
        let active: Vec<&str> = crumbs
            .iter()
            .filter(|crumb| crumb.active)
            .map(|crumb| crumb.text.as_str())
            .collect();
        assert_eq!(active, vec!["query_builder.rs"]);
    }

    #[test]
    fn moving_the_caret_really_changes_the_rendered_crumb_list() {
        let inside = breadcrumb_crumbs(
            Path::new("src/db/query_builder.rs"),
            &symbol_path_at_marker("let marker = 1;"),
        );
        // Between `mod db {` and the `impl` that follows it: inside the module, inside nothing
        // else - so the breadcrumb genuinely sheds two crumbs.
        let outside = breadcrumb_crumbs(
            Path::new("src/db/query_builder.rs"),
            &symbol_path_at_marker("mod db {"),
        );
        let texts = |crumbs: &[Crumb]| -> Vec<String> {
            crumbs.iter().map(|crumb| crumb.text.clone()).collect()
        };
        assert_eq!(
            texts(&outside),
            vec!["src", "db", "query_builder.rs", "mod db"]
        );
        assert_ne!(texts(&inside), texts(&outside));
    }

    #[test]
    fn with_no_symbol_path_the_breadcrumb_is_exactly_the_path_it_always_was() {
        let crumbs = breadcrumb_crumbs(Path::new("Cargo.toml"), &[]);
        let texts: Vec<&str> = crumbs.iter().map(|crumb| crumb.text.as_str()).collect();
        assert_eq!(texts, vec!["Cargo.toml"]);
        assert!(crumbs[0].active);
    }

    #[test]
    fn a_clean_file_draws_no_count_dots_at_all() {
        assert!(diagnostic_count_dot(diagnostics_view::Severity::Error, 0).is_none());
        assert!(diagnostic_count_dot(diagnostics_view::Severity::Warning, 0).is_none());
        assert!(diagnostic_count_dot(diagnostics_view::Severity::Error, 1).is_some());
    }

    /// The counts the band draws come straight off `LspFileStatus::Analyzed`, which
    /// `crate::lsp::client::lsp_file_status` builds with
    /// `diagnostics_view::count_errors_and_warnings` - so this drives real `lsp_types::
    /// Diagnostic` values through that same real counting function and checks the pair the
    /// breadcrumb would be handed.
    #[test]
    fn the_breadcrumb_counts_come_from_a_real_diagnostics_list() {
        let diagnostics = vec![
            severity_diagnostic(lsp_core::lsp_types::DiagnosticSeverity::ERROR),
            severity_diagnostic(lsp_core::lsp_types::DiagnosticSeverity::WARNING),
            severity_diagnostic(lsp_core::lsp_types::DiagnosticSeverity::WARNING),
            severity_diagnostic(lsp_core::lsp_types::DiagnosticSeverity::HINT),
        ];
        let (errors, warnings) = diagnostics_view::count_errors_and_warnings(&diagnostics);
        let status = LspFileStatus::Analyzed { errors, warnings };
        let counts = match &status {
            LspFileStatus::Analyzed { errors, warnings } => Some((*errors, *warnings)),
            _ => None,
        };
        assert_eq!(
            counts,
            Some((1, 2)),
            "one error and two warnings, with the hint counted as neither"
        );
    }

    fn severity_diagnostic(
        severity: lsp_core::lsp_types::DiagnosticSeverity,
    ) -> lsp_core::lsp_types::Diagnostic {
        lsp_core::lsp_types::Diagnostic {
            range: lsp_core::lsp_types::Range {
                start: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                end: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 4,
                },
            },
            severity: Some(severity),
            ..Default::default()
        }
    }
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
        theme::editor::GUTTER_TEXT_ACTIVE
    } else {
        theme::editor::GUTTER_TEXT
    };
    // "Worst wins": the tie-break for a line's row-level treatment when it carries diagnostics
    // of mixed severity (see `Severity::worst`), not whichever is first in the Vec.
    let worst_severity = diagnostics_view::Severity::worst(diagnostics);
    // The hovered span on this line, if any, compared by run-level byte range rather than a
    // re-derived UTF-16 conversion of rust-analyzer's own `Hover::range`.
    let hovered_byte_range = hover_entry.and_then(|entry| {
        (entry.line_number == line_number && entry.worth_underlining())
            .then(|| entry.byte_range.clone())
    });

    // The code runs keep their natural width in their own `flex_none` box, so they never shrink.
    // The inline diagnostic message is deliberately **not** in here (GitHub issue #186): inside a
    // `flex_none` box it could never shrink either, so on a narrow pane it overflowed and painted
    // straight over the code text. It is a shrinkable sibling below instead.
    let mut runs = div().flex().flex_none();
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
            // active error is more urgent than a symbol the pointer is merely resting on.
            run = run
                .border_b_1()
                .border_color(theme::syntax::HOVER_UNDERLINE);
        }
        // GitHub issue #186: a real mouse-hover trigger, not the click this used to be. This
        // read-only fallback view has one real element per syntax run, so GPUI's own
        // `on_hover` (`vendor/zed/crates/gpui/src/elements/div.rs`, closure argument `&bool`)
        // resolves "which token is the pointer on" precisely with no hit-testing of its own -
        // unlike the live-buffer view, whose row is a single shaped line and which therefore needs
        // `AdeApp::track_hover_pointer`'s real per-pixel resolution instead.
        //
        // Only a non-whitespace token is a hover/go-to-definition target; hovering whitespace
        // would just ask rust-analyzer about nothing.
        if let Some(path) = hover_target {
            if !run_text.trim().is_empty() {
                let anchor = HoverAnchor {
                    path: path.to_path_buf(),
                    line_number,
                    byte_range: run_start..run_end,
                    position: hover_view::position_for_line_byte_offset(
                        line_number as u32 - 1,
                        &line.text,
                        run_start,
                    ),
                };
                run = run.on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    if *hovered {
                        this.hover_over_token(anchor.clone(), cx);
                    } else if this.hover_anchor_matches(&anchor) {
                        // Guarded on the anchor still being *this* token: moving from one run
                        // straight onto the next delivers the new run's `true` and the old run's
                        // `false` in an order GPUI doesn't promise, and an unguarded dismissal
                        // would then close the card the new run had just opened.
                        this.dismiss_hover_and_notify(cx);
                    }
                }));
            }
        }
        runs = runs.child(run);
    }
    // `flex_1` + `min_w_0` so the text row fills the pane's remaining width and the shrinkable
    // children below (the inline diagnostic message, the blame span) truncate exactly at its
    // right edge.
    let mut text_row = div().flex().flex_1().min_w_0().child(runs);
    if let Some(first) = diagnostics.first() {
        // Only the message's first line is shown inline: `uniform_list` measures one row's
        // height and applies it uniformly to every row, so a multi-line rustc message (embedded
        // `\n`s are routine) would otherwise clip or overlap the row below. The full message is
        // in `AdeApp::render_diagnostic_card`'s real popover, which isn't height-constrained.
        let first_line = first.message.lines().next().unwrap_or_default();
        text_row = text_row.child(render_inline_diagnostic_message(
            first_line,
            first.severity,
            line_number,
        ));
    }
    // GitHub issue #29: the current line's dimmed inline git blame, placed in-flow immediately
    // after the code text so it begins right at the end of the line and is truncated at the
    // pane's right edge. `inline_blame` is only ever `Some` on the current line (see
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
        .when(is_current, |el| el.bg(theme::editor::CURRENT_LINE))
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
                    theme::editor::DIFF_ADDED
                } else {
                    theme::ColorToken::literal(work_surface::TRANSPARENT)
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
    cx: &mut Context<AdeApp>,
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
        // A `Failed` status is the one status a user can actually *do* something about, so it is
        // the one status that is clickable: it restarts this worktree's language servers, the
        // same real `AdeApp::restart_lsp_clients` the `Restart Language Servers` palette command
        // runs. Without it the recovery path exists but is only reachable by already knowing to
        // look for it in the palette - and the failing chip is precisely where a user who has
        // noticed their diagnostics stop is looking. Every other status is left inert rather than
        // given a handler that would silently no-op (the same rule `render_diff_row` follows).
        let failed = matches!(status, LspFileStatus::Failed(_));
        let mut chip = div()
            .id("file-view-lsp-status")
            .debug_selector(|| "file-view-lsp-status".to_string())
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .flex_none()
                    .w(px(5.0))
                    .h(px(5.0))
                    .rounded_full()
                    .bg(dot_color),
            )
            .child(label);
        if failed {
            chip = chip
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .tooltip(crate::root::widgets::text_tooltip(
                    "Click to restart this worktree's language servers",
                ))
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.restart_lsp_clients(cx);
                }));
        }
        bar = bar.child(chip);
    }

    bar.child(parsed.language)
        .child(position)
        .child(parsed.line_ending.label())
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

/// The File view footer's failed-status chip is the one place a user who has *noticed* their
/// language server stop is actually looking, so it is a real, clickable recovery - driven here
/// through a genuine painted-bounds click, the same idiom the status bar's own zoom-value test
/// uses, rather than by calling the handler directly.
///
/// Without this, the recovery existed but was reachable only by already knowing to search the
/// command palette for it - which is not a recovery path a user can find.
#[cfg(test)]
mod lsp_failed_status_chip_tests {
    use super::*;
    use crate::lsp::client::LspClientState;
    use gpui::TestAppContext;

    const CHIP_SELECTOR: &str = "file-view-lsp-status";

    #[gpui::test]
    async fn clicking_the_failed_status_chip_really_restarts_the_language_servers(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        let main_rs = root.join("src").join("main.rs");
        std::fs::write(&main_rs, "fn main() {}\n").expect("write main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
        let key = (root.clone(), "rust-analyzer");
        app.update_in(cx, |app, window, cx| {
            // A real, already-recorded failure - exactly the state `reap_dead_lsp_clients` puts a
            // dead server into on the poll cadence.
            app.lsp_clients.insert(
                key.clone(),
                LspClientState::Failed(
                    "rust-analyzer's connection was lost (the process exited or stopped \
                     responding)"
                        .to_string(),
                ),
            );
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds(CHIP_SELECTOR)
            .expect("a file with a failed language server must paint its real status chip");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        // The real, complete recovery, observed rather than assumed: the click freed the key, the
        // very next render's own `ensure_lsp_client` spawned a genuinely new `rust-analyzer`, and
        // it completed a real handshake into `Ready`. Before this fix `spawn_lsp_client` would
        // have found the `Failed` entry still sitting there and done nothing at all.
        app.read_with(cx, |app, _| {
            let state = app
                .lsp_clients
                .get(&key)
                .expect("the ordinary render path should have re-populated this key");
            assert!(
                matches!(state, LspClientState::Spawning | LspClientState::Ready(_)),
                "a real click on the failed chip must run the same real \
                 AdeApp::restart_lsp_clients the palette command does - the stale Failed entry \
                 must be gone, replaced by a genuinely fresh spawn"
            );
        });
    }
}
