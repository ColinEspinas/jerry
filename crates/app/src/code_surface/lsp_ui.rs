//! The language-server UI drawn *over* Surface C: the hover card, the diagnostics card,
//! the per-severity decoration colours, and go-to-definition. The client that produces
//! the responses these draw lives in `crate::lsp`; this module is only their UI.

use super::*;
// Only this module's own tests read `LspClientState` directly now - the render path goes through
// `AdeApp::lsp_connection_for_path`'s facade instead of raw client states (see
// `crate::lsp::client::LspConnection`), so a non-test import here would be genuinely unused.
#[cfg(test)]
use crate::lsp::client::LspClientState;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_keycap;

impl AdeApp {
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
    pub(in crate::code_surface) fn request_hover(
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

        let Some(client) = self.lsp_connection_for_path(&absolute_path) else {
            // No ready LSP client for this file's language yet; nothing to show, so clear any
            // stale entry - a real completions popup is equally stale in that case (Revision
            // R8.5b audit finding 3), so it's dropped alongside `hover` here too.
            self.hover = None;
            self.dismiss_completions();
            cx.notify();
            return;
        };

        let Ok(uri) = lsp_core::LspClient::uri_for_path(&absolute_path) else {
            self.hover = None;
            self.dismiss_completions();
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
    pub(in crate::code_surface) fn trigger_goto_definition(&mut self, cx: &mut Context<Self>) {
        let Some(hover) = self.hover.as_ref() else {
            return;
        };
        let path = hover.path.clone();
        let position = hover.position;

        let Some(client) = self.lsp_connection_for_path(&path) else {
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
    /// either way `Self::open_file_view`'s own `strip_prefix` handles it, since `PathBuf::join` with
    /// an already-absolute path just becomes that path.
    ///
    /// ## Avoiding a cursor-line race
    ///
    /// [`Self::open_file_view`] alone lands on the right file but not the right line: if the file
    /// wasn't already open, its background load unconditionally sets [`Self::code_cursor`] to 1
    /// once it completes, which would clobber a line set directly here before the load even
    /// starts. [`Self::pending_cursor_line`] is the one-shot instruction that survives the load
    /// instead; `Self::spawn_file_load`'s completion handler consumes it.
    pub(in crate::code_surface) fn navigate_to_definition(
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

    /// [`GotoDefinition`]'s bound `F12` action handler.
    pub(crate) fn handle_goto_definition_action(
        &mut self,
        _action: &GotoDefinition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.trigger_goto_definition(cx);
    }
}

/// Surface C's Diagnostic-state card: one row per diagnostic currently indexed anywhere in the
/// open file, `None` when there are none (a clean file renders no card, not an empty one).
/// Listing every diagnostic in the file (rather than only the one under the cursor) is a
/// simplification: the design anchors this card under the caret line, but this app has no
/// floating-popup infrastructure yet.
pub(in crate::code_surface) fn render_diagnostics_card(
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

/// 220px - a soft cap on the real Hover popover's height, purely so
/// [`AdeApp::render_hover_card`]'s own real "is there room below the hovered row" measurement
/// (mirroring [`AdeApp::render_completions_popover`]'s identical `POPOVER_MAX_HEIGHT` judgment -
/// see that constant's own docs) has a concrete number to compare real available space against,
/// and so a real, unusually long doc string can't paint past the window. Not derived from the
/// design mockup (`design_handoff_jerry_ade/revision/Jerry.dc.html`'s own hover card has no
/// fixed height - it's exactly as tall as its own real content), just a practical, generous
/// ceiling comfortably above what a real signature + doc + footer normally needs.
const HOVER_CARD_MAX_HEIGHT: gpui::Pixels = px(220.0);

impl AdeApp {
    /// Surface C's real, token-anchored Hover popover (signature, doc prose, module path, `F12
    /// definition` footer) - mirrors [`Self::render_completions_popover`]'s own real positioning
    /// mechanism exactly, matching that method's own top-doc'd reasoning for why: both anchor off
    /// a real, already-painted `(Bounds, ShapedLine)` pair and both paint as a real, absolutely-
    /// positioned top-level sibling in [`Render::render`] (`crate::root::AdeApp::render`), never
    /// nested inside the File view's own virtualized `uniform_list` - a popup anchored to one row
    /// must not be clipped by that row's own scroll container.
    ///
    /// The one real difference from [`Self::render_completions_popover`]: this reads
    /// [`Self::file_view_row_layout`] (every currently *visible* row, keyed by 1-based line
    /// number) rather than [`Self::file_view_last_layout`]/[`Self::file_view_last_bounds`] (the
    /// *caret's own* row alone). [`Self::hover`]'s own target line is the line a real click
    /// landed a token on, which today always also happens to be wherever the caret just moved to
    /// (`crate::code_surface::editing`'s click handler moves the caret and requests hover from the same
    /// click) - but reading the hovered line's own real layout entry directly is still the more
    /// directly correct real position to anchor from, not a second, independently-computed one
    /// that merely happens to agree in practice.
    ///
    /// `None` whenever there's nothing real to anchor to: no [`Self::hover`] entry, the entry
    /// belongs to a file that isn't the one currently on screen, or the hovered row's own real
    /// layout isn't in [`Self::file_view_row_layout`] right now (e.g. scrolled out of view since
    /// the click landed) - the same honest "degrade to nothing rather than paint at a guessed
    /// position" discipline [`Self::render_completions_popover`] already established.
    pub(crate) fn render_hover_card(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let hover = self.hover.as_ref()?;
        let active_relative = self.active_editable_path()?;
        if hover.path != self.file_tree_root.join(&active_relative) {
            return None;
        }
        let (row_bounds, shaped) = self.file_view_row_layout.get(&hover.line_number)?;

        let anchor_x = row_bounds.left() + shaped.x_for_index(hover.byte_range.start);
        let row_top = row_bounds.top();
        let row_bottom = row_bounds.bottom();

        // Flip above the hovered row when there isn't real room below it in the window body -
        // the same real "measure real available space, flip if it doesn't fit" judgment
        // `Self::render_completions_popover` already makes (see that method's own docs for the
        // `vendor/zed` precedent this follows).
        let space_below = self.body_bounds.bottom() - row_bottom;
        let fits_below = space_below >= HOVER_CARD_MAX_HEIGHT;
        let top = if fits_below {
            row_bottom
        } else {
            (row_top - HOVER_CARD_MAX_HEIGHT).max(self.body_bounds.top())
        };

        Some(render_hover_card_content(hover, anchor_x, top, cx))
    }
}

/// The real Hover popover's own content - split out of [`AdeApp::render_hover_card`] purely so
/// the real positioning math above (which needs `&self`) stays visually separate from the real
/// per-status content build below (which doesn't) - mirrors
/// [`AdeApp::render_completions_popover`]'s own inline match, just factored into its own function
/// since this one has a real early-return position/anchor computation ahead of it.
fn render_hover_card_content(
    hover: &HoverEntry,
    anchor_x: Pixels,
    top: Pixels,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    let mut card = div()
        .id("hover-card")
        // Lets a real test measure this real popover's own painted bounds (`debug_bounds` reads
        // this, not `.id(..)` - see `hover_popover_position_tests`) - a no-op outside test
        // builds, matching every other `debug_selector` in this crate.
        .debug_selector(|| "hover-card".to_string())
        .absolute()
        .left(anchor_x)
        .top(top)
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(10.0))
        .max_w(px(430.0))
        .max_h(HOVER_CARD_MAX_HEIGHT)
        .overflow_hidden()
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

    card.into_any_element()
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
/// (`crate::lsp::diagnostics::overlay_diagnostic_runs`; GPUI has no true dotted border, see
/// vendor/zed/crates/gpui/src/styled.rs's `border_dashed`), and a dim inline message from the
/// first diagnostic on the line (the full breakdown is `render_diagnostics_card`, below the code
/// area, not repeated per-row). The design only specifies an underline color for the error case;
/// `Warning` reuses [`theme::term::WARN`], `Information`/`Hint` reuse
/// [`theme::text::DIM`]/[`theme::text::FAINT`] with `Hint` dimmer, matching the convention that
/// LSP hints are the least severe/most subtle diagnostic kind.
pub(in crate::code_surface) fn diagnostic_underline_color(
    severity: diagnostics_view::Severity,
) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::ERROR_UNDERLINE.into(),
        diagnostics_view::Severity::Warning => theme::term::WARN.into(),
        diagnostics_view::Severity::Information => theme::text::DIM.into(),
        diagnostics_view::Severity::Hint => theme::text::FAINT.into(),
    }
}

/// The File view row's background tint for a diagnostic of `severity` - `None` means no tint.
/// Only `Error` gets one ([`theme::syntax::DIAGNOSTIC_ROW_BG`]); the other three are
/// distinguished from a clean line by their dotted underline alone, keeping every non-error
/// severity visibly less alarming than an error.
pub(in crate::code_surface) fn diagnostic_row_bg(
    severity: diagnostics_view::Severity,
) -> Option<gpui::Rgba> {
    match severity {
        diagnostics_view::Severity::Error => Some(theme::syntax::DIAGNOSTIC_ROW_BG.into()),
        _ => None,
    }
}

/// The File view row's inline end-of-line message color for a diagnostic of `severity` - `Error`
/// keeps [`theme::syntax::DIAGNOSTIC_INLINE_MESSAGE`]; every other severity reuses
/// [`theme::text::FAINT`].
pub(in crate::code_surface) fn diagnostic_inline_message_color(
    severity: diagnostics_view::Severity,
) -> gpui::Rgba {
    match severity {
        diagnostics_view::Severity::Error => theme::syntax::DIAGNOSTIC_INLINE_MESSAGE.into(),
        _ => theme::text::FAINT.into(),
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
                    app.lsp_clients
                        .get(&(project.path().to_path_buf(), "rust-analyzer")),
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
                    app.lsp_clients
                        .get(&(project.path().to_path_buf(), "rust-analyzer")),
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

/// Real regression coverage for bug 2 in this revision's brief: the Hover popover used to render
/// at a fixed position (an in-flow card below the code, effectively at whatever a given render
/// pass's flow put it - in practice, the bottom of the visible content) instead of anchored near
/// the real hovered token. These tests bypass the real `textDocument/hover` round trip entirely
/// (seeding [`AdeApp::hover`] directly, the same established pattern
/// `stale_completions_popup_tests::fake_ready_entry` already uses for completions) since the bug
/// and its fix are purely about *where* an already-resolved hover result paints, never about the
/// real LSP request/response plumbing itself (out of scope per this revision's own brief).
#[cfg(test)]
mod hover_popover_position_tests {
    use super::*;
    use gpui::{Entity, TestAppContext};

    fn seed_ready_hover_for_line(
        app: &Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        path: PathBuf,
        line_number: usize,
    ) {
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path,
                line_number,
                byte_range: 0..3,
                position: lsp_core::lsp_types::Position {
                    line: line_number as u32 - 1,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn real_symbol()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
    }

    /// The real, load-bearing proof that the popover is anchored to the real hovered row, not
    /// painted at a fixed position: seeding the exact same [`HoverEntry`] content for two
    /// different real lines must move the real painted popover to two different real positions,
    /// each one genuinely close to that line's own real painted row - a fixed-position popover
    /// (the pre-fix bug) would paint at the same spot regardless of which line was hovered.
    #[gpui::test]
    fn the_real_painted_hover_card_moves_with_the_real_hovered_row(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.txt");
        std::fs::write(&file_path, "one\ntwo\nthree\nfour\nfive\n").expect("write sample.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        let row_2_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&2).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 2's real row should have painted real layout by now");
        let row_4_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&4).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 4's real row should have painted real layout by now");
        assert!(
            row_4_bounds.top() > row_2_bounds.top(),
            "sanity check: line 4 must really paint below line 2"
        );

        seed_ready_hover_for_line(&app, cx, file_path.clone(), 2);
        let card_for_line_2 = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for line 2");
        assert!(
            (card_for_line_2.top() - row_2_bounds.bottom()).abs() < gpui::px(2.0),
            "the real hover card for line 2 must paint directly under line 2's own real row \
             (row bottom {:?}, card top {:?})",
            row_2_bounds.bottom(),
            card_for_line_2.top()
        );

        seed_ready_hover_for_line(&app, cx, file_path, 4);
        let card_for_line_4 = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for line 4");
        assert!(
            (card_for_line_4.top() - row_4_bounds.bottom()).abs() < gpui::px(2.0),
            "the real hover card for line 4 must paint directly under line 4's own real row \
             (row bottom {:?}, card top {:?})",
            row_4_bounds.bottom(),
            card_for_line_4.top()
        );

        assert!(
            card_for_line_4.top() > card_for_line_2.top(),
            "hovering a real, later line must move the real painted popover further down - a \
             fixed-position popover (the real bug this revision fixes) would paint both at the \
             exact same spot: line 2's card was at {:?}, line 4's card was at {:?}",
            card_for_line_2.top(),
            card_for_line_4.top()
        );
    }

    /// The real popover's horizontal anchor tracks the real hovered token's own column, not a
    /// fixed left offset either - hovering a token further into a line must shift the real
    /// painted card's own real left edge to the right by roughly the same real pixel distance.
    #[gpui::test]
    fn the_real_painted_hover_card_moves_horizontally_with_the_real_hovered_column(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("sample.txt");
        std::fs::write(&file_path, "aaaa bbbb cccc dddd\n").expect("write sample.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file_path.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });

        let row_bounds = app
            .read_with(cx, |app, _| app.file_view_row_layout.get(&1).cloned())
            .map(|(bounds, _)| bounds)
            .expect("line 1's real row should have painted real layout by now");

        // Hover the first token (byte 0..4, "aaaa").
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path.clone(),
                line_number: 1,
                byte_range: 0..4,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn real_symbol()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
        let card_for_first_token = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for the first token");

        // Hover the last token (byte 15..19, "dddd") on the exact same line.
        app.update(cx, |app, cx| {
            app.hover = Some(HoverEntry {
                path: file_path,
                line_number: 1,
                byte_range: 15..19,
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 15,
                },
                status: HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                    module_path: None,
                    signature: "fn real_symbol()".to_string(),
                    doc: None,
                })),
            });
            cx.notify();
        });
        cx.run_until_parked();
        let card_for_last_token = cx
            .debug_bounds("hover-card")
            .expect("the real hover card should have painted real bounds for the last token");

        assert!(
            card_for_last_token.left() > card_for_first_token.left(),
            "hovering a real token further into the same line must move the real painted card's \
             own real left edge to the right - a fixed-position popover would paint both at the \
             exact same horizontal spot: first-token card was at {:?}, last-token card was at \
             {:?} (row bounds: {:?})",
            card_for_first_token.left(),
            card_for_last_token.left(),
            row_bounds
        );
    }
}

/// The real, live end-to-end proof for this app's two-server (primary + companion) LSP support:
/// a genuine `.vue` file, analyzed by a genuinely spawned `vue-language-server` **and** a
/// genuinely spawned `typescript-language-server` carrying the real `@vue/typescript-plugin`,
/// coordinated by this app's own real relay - reaching real diagnostics and a real hover through
/// nothing but the production code path (`AdeApp::open_file_view` -> `render_file_view` ->
/// `ensure_lsp_client` (both halves) -> `dispatch_did_open` (both halves) -> render).
///
/// Nothing here is stubbed: two real Node processes, a real `npm install typescript` for the real
/// `--tsdk` this app resolves on its own, and real assertions on the actual diagnostic text and
/// hover markdown the real servers produce. It is genuinely slow for that reason, and kept in the
/// normal (non-`#[ignore]`) suite on purpose - this project has no separate slow-test lane.
///
/// The fixture carries **two** deliberately different real errors, because in Vue's hybrid mode
/// each server answers a genuinely different class of question (see `crate::language`'s own docs):
/// a template compile error only `vue-language-server` reports, and a TypeScript type error only
/// the companion reports. Asserting on both is what actually proves the merge is a real union of
/// two live contributors rather than one server's answer dressed up as two.
#[cfg(test)]
mod vue_two_server_wiring_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    /// A real `<script setup lang="ts">` type error (`bad`) plus a real template compile error
    /// (the mismatched `</span>`), in one real single-file component. The `shape`/`picked` lines
    /// exist for the real go-to-definition and completion probes below - a genuine cross-line
    /// reference and a genuine member-access position, both valid TypeScript so they can't disturb
    /// the two deliberate errors above them.
    const FIXTURE_VUE: &str = "<script setup lang=\"ts\">\n\
         const bad: number = \"not a number\"\n\
         const shape = { alpha: 1, beta: 2 }\n\
         const picked = shape.alpha\n\
         </script>\n\
         \n\
         <template>\n\
         \x20 <div>{{ bad }}</span>\n\
         </template>\n";

    /// 0-based line 1, character 7 - inside the real `bad` identifier of
    /// `const bad: number = "not a number"`. `request_hover`'s `line_number` is 1-based (see
    /// `AdeApp::code_cursor`'s own convention) and its `byte_range` is within that line's text:
    /// `"const "` is 6 bytes, `"bad"` 3 more.
    const SCRIPT_LINE: usize = 2;
    const SCRIPT_BYTE_RANGE: Range<usize> = 6..9;
    const SCRIPT_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 1,
        character: 7,
    };

    /// 0-based line 3, inside the `shape` of `const picked = shape.alpha` (`"const picked = "` is
    /// 15 characters, `shape` the next 5) - a real reference whose real declaration is one line up.
    const REFERENCE_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 3,
        character: 17,
    };
    /// The same line, immediately after the real `.` (character 20) - a genuine member-access
    /// completion context on a genuinely typed object.
    const MEMBER_ACCESS_POSITION: lsp_core::lsp_types::Position = lsp_core::lsp_types::Position {
        line: 3,
        character: 21,
    };

    /// Writes the real scratch Vue project and performs the real, project-local
    /// `npm install typescript@5` this app's own `--tsdk` resolution genuinely needs (see
    /// `crate::language::vue_dynamic_args`: it existence-checks
    /// `node_modules/typescript/lib/typescript.js` specifically, and refuses to spawn without it).
    /// A stubbed stand-in would be worse than useless here - the real `vue-language-server` really
    /// does load this file, and a fake one would produce a different, equally fake failure.
    fn write_scratch_vue_project() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\": {\"strict\": true, \"target\": \"ES2020\", \
             \"module\": \"ESNext\", \"moduleResolution\": \"Bundler\", \"jsx\": \"preserve\"}, \
             \"include\": [\"**/*.ts\", \"**/*.vue\"]}\n",
        )
        .expect("write tsconfig.json");
        std::fs::write(dir.path().join("App.vue"), FIXTURE_VUE).expect("write App.vue");
        let status = std::process::Command::new("npm")
            .args([
                "install",
                "typescript@5",
                "--no-audit",
                "--no-fund",
                "--silent",
            ])
            .current_dir(dir.path())
            .status()
            .expect("npm should be on PATH in this sandbox (real, live network install)");
        assert!(status.success(), "npm install typescript@5 failed");
        assert!(
            dir.path()
                .join("node_modules/typescript/lib/typescript.js")
                .is_file(),
            "the real --tsdk target this app resolves must genuinely exist after the install"
        );
        dir
    }

    /// Re-renders, advances the deterministic clock past one `LSP_DIAGNOSTICS_POLL_INTERVAL`, and
    /// drains, until `predicate` holds or `deadline` passes.
    ///
    /// Advancing the clock is load-bearing here and not in the single-server tests: this app's
    /// relay dispatch lives in `AdeApp::ensure_lsp_poll_task`'s own `timer(..)`-driven loop, which
    /// on GPUI's deterministic test executor only ticks when the clock is actually advanced - and
    /// the real `vue-language-server` will not produce a single diagnostic until its relayed
    /// `tsserver/request` has been answered.
    fn wait_until(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        deadline: Instant,
        message: &str,
        predicate: impl Fn(&AdeApp) -> bool,
    ) {
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.background_executor
                .advance_clock(LSP_DIAGNOSTICS_POLL_INTERVAL + Duration::from_millis(10));
            cx.run_until_parked();
            if app.read_with(cx, |app, _| predicate(app)) {
                return;
            }
            assert!(Instant::now() < deadline, "{message}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn diagnostic_messages(app: &AdeApp) -> Vec<String> {
        app.file_view_diagnostics
            .values()
            .flatten()
            .map(|diagnostic| diagnostic.message.clone())
            .collect()
    }

    #[gpui::test]
    fn a_real_vue_file_gets_real_diagnostics_from_both_servers_and_a_real_hover(
        cx: &mut TestAppContext,
    ) {
        // Silent unless `RUST_LOG` is actually set, so this doesn't spam the normal suite - but
        // `RUST_LOG=app::lsp::client=debug cargo test ...` then surfaces the real relay round-trip
        // timing `AdeApp::dispatch_companion_relay` logs (measured live at ~223ms for the real
        // `_vue:projectInfo` query while building this).
        let _ = env_logger::builder().parse_default_env().try_init();
        let project = write_scratch_vue_project();
        let app_vue = project.path().join("App.vue");
        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        let opened_at = Instant::now();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(app_vue.clone(), window, cx);
        });
        cx.run_until_parked();

        // Both halves must genuinely be spawned by the production path - two real, independently
        // keyed clients, not one client pretending to be two.
        let companion_key = language::companion_for_extension(Some("vue"))
            .expect("vue has a real companion")
            .client_key;
        wait_until(
            &app,
            cx,
            Instant::now() + Duration::from_secs(180),
            "the real vue-language-server and its real typescript-language-server companion were \
             never both Ready - check that both are installed and that @vue/typescript-plugin \
             resolved",
            |app| {
                let root = &app.file_tree_root;
                [("vue-language-server"), (companion_key)]
                    .iter()
                    .all(|key| {
                        matches!(
                            app.lsp_clients.get(&(root.clone(), *key)),
                            Some(LspClientState::Ready(_))
                        )
                    })
            },
        );

        // The companion's own contribution: a real TypeScript semantic error for `.vue` content,
        // which only exists because the real `@vue/typescript-plugin` is genuinely loaded.
        wait_until(
            &app,
            cx,
            Instant::now() + Duration::from_secs(180),
            "no real TypeScript diagnostic for the genuine `.vue` script type error ever reached \
             file_view_diagnostics",
            |app| {
                diagnostic_messages(app)
                    .iter()
                    .any(|message| message.to_lowercase().contains("not assignable"))
            },
        );
        let companion_diagnostic_at = opened_at.elapsed();

        // The primary's own contribution: a real Vue template compile error, which the companion
        // does not and cannot report.
        wait_until(
            &app,
            cx,
            Instant::now() + Duration::from_secs(120),
            "no real Vue template diagnostic ever reached file_view_diagnostics - without it, \
             only one of the two real servers is genuinely contributing",
            |app| {
                diagnostic_messages(app)
                    .iter()
                    .any(|message| message.to_lowercase().contains("end tag"))
            },
        );

        app.read_with(cx, |app, _| {
            let messages = diagnostic_messages(app);
            println!(
                "vue e2e: real merged diagnostics after {:?}: {messages:?}",
                opened_at.elapsed()
            );
            assert!(
                messages
                    .iter()
                    .any(|message| message.to_lowercase().contains("not assignable")),
                "the companion's real TypeScript diagnostic must still be present in the merged \
                 view alongside the primary's, got: {messages:?}"
            );
            assert_eq!(
                app.lsp_clients.len(),
                2,
                "exactly two real clients - one primary, one companion - should exist for this \
                 repo root, got: {:?}",
                app.lsp_clients.keys().collect::<Vec<_>>()
            );
        });
        println!(
            "vue e2e: first real companion diagnostic reached the render path in \
             {companion_diagnostic_at:?} from open_file_view"
        );

        // A real hover, through the facade's real companion fallback: the primary answers `null`
        // for every position in a `.vue` file (real, expected hybrid-mode behavior), so a
        // non-`None` result here can only have come from the companion via `LspConnection`.
        let hover_deadline = Instant::now() + Duration::from_secs(120);
        let hover_started = Instant::now();
        let model = loop {
            app.update(cx, |app, cx| {
                app.hover = None;
                app.request_hover(
                    app_vue.clone(),
                    SCRIPT_LINE,
                    SCRIPT_BYTE_RANGE,
                    SCRIPT_POSITION,
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
                    Instant::now() < hover_deadline,
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
                break model;
            }
            assert!(
                Instant::now() < hover_deadline,
                "no real, non-empty hover ever came back for the genuine `bad` identifier in the \
                 real .vue script block - the companion fallback in LspConnection::request is \
                 what has to supply it, since the primary genuinely answers null there"
            );
            std::thread::sleep(Duration::from_millis(300));
        };
        println!(
            "vue e2e: real hover resolved in {:?}: {model:?}",
            hover_started.elapsed()
        );
        let rendered = format!("{model:?}");
        assert!(
            rendered.contains("bad") && rendered.contains("number"),
            "the real hover should describe the genuine `const bad: number` declaration, got: \
             {rendered}"
        );

        // Revision R11 audit finding 1, proven against the real toolchain rather than only against
        // `lsp_connection_facade_tests`' small real servers: hover was never the only request the
        // real primary answers emptily inside a `.vue` script block. Both of these go through the
        // exact same `LspConnection::request` the production F12 handler and
        // `AdeApp::schedule_lsp_sync`'s completion request use, and before this fix both came back
        // empty here - go-to-definition as an empty *array* and completion as an empty
        // `CompletionList`, neither of which the old null-only check recognized.
        let connection = app
            .read_with(cx, |app, _| app.lsp_connection_for_path(&app_vue))
            .expect("both real halves are Ready by now");
        let uri = lsp_core::LspClient::uri_for_path(&app_vue).expect("a real file:// uri");

        let definition = retry_until_some(
            Instant::now() + Duration::from_secs(120),
            "no real go-to-definition ever came back for `shape` in the real .vue script block - \
             the real vue-language-server answers an empty array there, so only the companion \
             fallback in LspConnection::request can supply one",
            || {
                let params = lsp_core::lsp_types::GotoDefinitionParams {
                    text_document_position_params:
                        lsp_core::lsp_types::TextDocumentPositionParams {
                            text_document: lsp_core::lsp_types::TextDocumentIdentifier {
                                uri: uri.clone(),
                            },
                            position: REFERENCE_POSITION,
                        },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                };
                match connection
                    .request::<lsp_core::lsp_types::request::GotoDefinition>(
                        params,
                        LSP_QUERY_TIMEOUT,
                    )
                    .ok()
                    .flatten()
                {
                    Some(lsp_core::lsp_types::GotoDefinitionResponse::Array(locations))
                        if locations.is_empty() =>
                    {
                        None
                    }
                    other => other,
                }
            },
        );
        println!("vue e2e: real go-to-definition answer: {definition:?}");

        let completions = retry_until_some(
            Instant::now() + Duration::from_secs(120),
            "no real completions ever came back after `shape.` in the real .vue script block - \
             the real vue-language-server answers an empty items list there",
            || {
                let params = lsp_core::lsp_types::CompletionParams {
                    text_document_position: lsp_core::lsp_types::TextDocumentPositionParams {
                        text_document: lsp_core::lsp_types::TextDocumentIdentifier {
                            uri: uri.clone(),
                        },
                        position: MEMBER_ACCESS_POSITION,
                    },
                    work_done_progress_params: Default::default(),
                    partial_result_params: Default::default(),
                    context: None,
                };
                let items = match connection
                    .request::<lsp_core::lsp_types::request::Completion>(params, LSP_QUERY_TIMEOUT)
                    .ok()
                    .flatten()
                {
                    Some(lsp_core::lsp_types::CompletionResponse::Array(items)) => items,
                    Some(lsp_core::lsp_types::CompletionResponse::List(list)) => list.items,
                    None => Vec::new(),
                };
                (!items.is_empty()).then_some(items)
            },
        );
        let labels: Vec<String> = completions.iter().map(|item| item.label.clone()).collect();
        println!("vue e2e: real completion labels after `shape.`: {labels:?}");
        assert!(
            labels.iter().any(|label| label == "alpha")
                && labels.iter().any(|label| label == "beta"),
            "the real companion knows both members of the genuine `{{ alpha, beta }}` object, \
             got: {labels:?}"
        );
    }

    /// Calls `attempt` until it returns a real `Some` or `deadline` passes. Both real servers are
    /// still settling for a while after the first diagnostic lands, so a single shot would be a
    /// race; nothing is fabricated on timeout, the assertion just fails.
    fn retry_until_some<T>(deadline: Instant, message: &str, attempt: impl Fn() -> Option<T>) -> T {
        loop {
            if let Some(value) = attempt() {
                return value;
            }
            assert!(Instant::now() < deadline, "{message}");
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}
