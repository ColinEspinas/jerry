use super::*;
use crate::code_surface::zoom::zoom_scoped;

impl AdeApp {
    /// Surface D - the merge-conflict resolution surface, replacing the pty/diff body below the
    /// tab strip and session context bar (which keep rendering normally) exactly like Surface
    /// B/C already do. Renders whichever [`merge::MergeFlowState`] `self.merge_flow` is
    /// currently in for `session`; every value shown (branch names, file paths, conflict line
    /// content) comes from the `wt_core::merge` call [`Self::start_merge`] made.
    ///
    /// Real per-token syntax coloring ([`code_view::highlight_block`], cached in
    /// [`Self::merge_highlight_cache`]) and a real two-column line-number gutter now render for
    /// the active hunk, matching the Diff view's own treatment
    /// (`code_surface::diff_view::AdeApp::render_diff_file_detail`). Unlike the Diff view's gutter (genuine
    /// old-file/new-file line numbers, since a diff hunk really does have two distinct files on
    /// either side), this one is **not** an old/new pair: both numbers are real line positions
    /// within the *same* conflicted working file - the file as it actually sits on disk, markers
    /// and all - so "old/new" framing would incorrectly suggest "ours' version of the file" vs.
    /// "theirs' version of the file" as two logically separate files, which they aren't here.
    /// The gutter is real, not fabricated: `wt_core::merge::ConflictHunk::ours_start_line`/
    /// `theirs_start_line` are captured while actually parsing the file's real
    /// `<<<<<<</=======/>>>>>>>` markers (the 1-indexed line immediately after each marker), so
    /// [`Self::render_conflict_columns`] just increments from a real anchor per rendered line -
    /// this surface used to omit the gutter entirely because no such anchor existed yet; it does
    /// now. [`Self::render_conflict_columns`] is a pure reader of [`Self::merge_highlight_cache`],
    /// and the cache itself is only ever (re)computed at a real state-transition point (see
    /// [`Self::ensure_active_merge_highlight_cache`]'s docs), never from this render path, so
    /// this whole surface stays `&self`.
    ///
    /// The left ("ours"/base) column is labelled with the base branch name rather than an agent
    /// identity, since `attempt_merge` always runs `git merge` from the base worktree - real git
    /// state, not a running session, so there's no agent to attribute a tint to.
    pub(crate) fn render_merge_flow_surface(
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
                    // `merge::first_unresolved` only ever points at a `ConflictedPath::Text`
                    // entry with a remaining `Conflict` segment, so both always match.
                    if let Some(ConflictedPath::Text(file)) = files.get(target_file) {
                        // Real hand-edit mode (Revision R8.5c): whenever `Self::merge_edit`
                        // matches the active file (by path), the whole-file editable view
                        // (`crate::merge::editing::AdeApp::render_merge_edit_view`) replaces
                        // the read-only two-column quick-pick view *and its Take-left/Take-right/
                        // Take-both buttons* entirely - not merely visually stacked alongside
                        // them. This structural exclusivity, not a shared-visibility toggle, is
                        // what rules out a quick-pick click racing an in-flight, unsaved hand-edit
                        // of the very same file/hunk: while hand-edit mode is on for this file,
                        // `Self::resolve_active_hunk`'s own button handlers simply have no button
                        // in the render tree to be clicked from at all.
                        let hand_editing = self
                            .merge_edit
                            .as_ref()
                            .is_some_and(|edit| edit.relative_path == file.relative_path);
                        if hand_editing {
                            body = body.child(self.render_merge_edit_view(cx));
                        } else if let Some(ConflictSegment::Conflict(hunk)) =
                            file.segments.get(target_hunk)
                        {
                            body = body
                                .child(self.render_conflict_columns(
                                    base_branch,
                                    session,
                                    &file.relative_path,
                                    hunk,
                                    cx,
                                ))
                                .child(self.render_take_both_row(cx))
                                .child(self.render_hand_edit_toggle_row(cx));
                        } else {
                            body = body.child(div().flex_1());
                        }
                    } else {
                        body = body.child(div().flex_1());
                    }
                } else {
                    // No text hunk left, but not resolved either: every remaining entry is a
                    // modify/delete or binary conflict with no text-hunk resolution action -
                    // a distinct panel rather than falling through to "conflicts resolved".
                    body =
                        body.child(self.render_unmergeable_panel(merge::unmergeable_paths(files)));
                }

                body.child(self.render_merge_flow_footer(resolved, self.merge_op_in_flight, cx))
                    .into_any_element()
            }
        }
    }

    /// Surface D's header row: `Resolve merge`, the base branch, and `hunk X of Y` for whichever
    /// file/hunk is active, computed from `crate::merge::state::hunk_position_in_file`/`hunk_count`.
    pub(in crate::merge) fn render_merge_header(
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

    /// Ensures [`Self::merge_highlight_cache`] holds real per-side syntax highlighting for
    /// `relative_path`'s `hunk` - recomputes (via [`code_view::highlight_block`]) only when the
    /// `(path, hunk)` pair differs from what's cached (real `PathBuf`/[`ConflictHunk`] equality
    /// checks - keyed on both, not the hunk alone, since highlighting depends on
    /// `relative_path`'s extension too; see [`Self::merge_highlight_cache`]'s own docs for the
    /// real, reachable bug keying on the hunk alone would allow).
    fn ensure_merge_highlight_cache(&mut self, relative_path: &Path, hunk: &ConflictHunk) {
        if self
            .merge_highlight_cache
            .as_ref()
            .is_some_and(|(cached_path, cached_hunk, _, _)| {
                cached_path == relative_path && cached_hunk == hunk
            })
        {
            return;
        }
        let extension = relative_path.extension().and_then(|ext| ext.to_str());
        let ours = code_view::highlight_block(hunk.ours.iter().map(String::as_str), extension);
        let theirs = code_view::highlight_block(hunk.theirs.iter().map(String::as_str), extension);
        self.merge_highlight_cache =
            Some((relative_path.to_path_buf(), hunk.clone(), ours, theirs));
    }

    /// Ensures [`Self::merge_highlight_cache`] is fresh for whichever conflict hunk is currently
    /// active in [`Self::merge_flow`] (a no-op if there's no active `Conflicted` hunk) - the
    /// real, change-triggered hook this cache is (re)computed from: called from
    /// `Self::start_merge`'s completion handler (a fresh `Conflicted` state) and
    /// [`Self::resolve_active_hunk`]'s advance-to-next-hunk point, **never** from `render()` -
    /// [`Self::render_conflict_columns`] only ever reads this cache. A stale cache there falls
    /// back to plain, uncoloured text for the affected lines until the next real transition
    /// recomputes it (see that method's own per-line fallback), rather than blocking the render
    /// call on a `tree-sitter` parse. Conflict hunk content is small (a real conflict *region*,
    /// not a whole file - unlike `wt_core::diff`'s hunks, `ConflictHunk` has no line cap, but a
    /// merge conflict's own size is inherently bounded by where two branches' edits actually
    /// overlap), so this stays a synchronous call at the real change point rather than a
    /// background `cx.spawn()` task, the same real-cost-justified choice
    /// `code_surface::diff_view::AdeApp::ensure_diff_highlight_cache`'s docs explain for the Diff view.
    pub(in crate::merge) fn ensure_active_merge_highlight_cache(&mut self) {
        let Some(flow) = self.merge_flow.as_ref() else {
            return;
        };
        let merge::MergeFlowState::Conflicted {
            files,
            active_file,
            active_hunk,
            ..
        } = &flow.state
        else {
            return;
        };
        let Some(ConflictedPath::Text(file)) = files.get(*active_file) else {
            return;
        };
        let Some(ConflictSegment::Conflict(hunk)) = file.segments.get(*active_hunk) else {
            return;
        };
        // Cloned out first (both cheap, real `Clone`s) so the call below doesn't have to hold
        // this borrow of `self.merge_flow` alive across it - the same "take it out first" shape
        // `Self::refresh_open_diff_file_cache` uses for the Diff view's own cache.
        let relative_path = file.relative_path.clone();
        let hunk = hunk.clone();
        self.ensure_merge_highlight_cache(&relative_path, &hunk);
    }

    /// Surface D's two-column split for the active conflict hunk - `ours`/`theirs` content
    /// extracted from the file's on-disk conflict markers, real per-token syntax coloring (a
    /// pure read of [`Self::merge_highlight_cache`], kept fresh by
    /// [`Self::ensure_active_merge_highlight_cache`]), a real gutter
    /// (`hunk.ours_start_line`/`theirs_start_line` plus each line's index), and real editor zoom
    /// (`zoom_scoped`, matching the Diff/File views). See [`Self::render_merge_flow_surface`]'s
    /// docs for why the left column is labelled with the base branch rather than an agent
    /// identity, and for where the gutter's real position data comes from.
    ///
    /// Row count for each column is driven by `hunk.ours`/`hunk.theirs` themselves - the real
    /// ground truth - never by the cached `RenderedLine` `Vec`'s own length: a genuinely empty
    /// side (real, reachable - base deletes a line, feature edits it) must render zero rows, and
    /// a cache that hasn't caught up yet (or doesn't match `relative_path`/`hunk`) must still
    /// show every real line (in plain, uncoloured text - see the per-line fallback below),
    /// rather than either fabricating a phantom row or silently hiding real conflict content.
    pub(in crate::merge) fn render_conflict_columns(
        &self,
        base_branch: &str,
        session: &Session,
        relative_path: &Path,
        hunk: &ConflictHunk,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // A real, deliberate contrast fix: an earlier revision distinguished "theirs" from
        // "ours" with a full-height `agent_bg` wash under the right column's text. Once real
        // syntax coloring (`code_view::color_for_kind`) started owning that text, a real
        // contrast check found `theme::syntax::COMMENT` against every real agent's `agent_bg`
        // (`work_surface::agent_tint`) lands around 2.2-2.5:1 - well under WCAG AA's 4.5:1 for
        // normal text - and the two columns wouldn't even match in readability, since the left
        // column never had a wash under its text. Fixing this by narrowing the tint to a 3px
        // left-edge accent bar (`agent_fg`, below) rather than adjusting `COMMENT` itself: that
        // keeps `theme::syntax::COMMENT` a single real, unconditional token wherever it's used
        // (the File/Diff views included) instead of a merge-view-specific override, and gets
        // both columns to the same real legibility - neither has text sitting on a color wash.
        let (agent_fg, _) = work_surface::agent_tint(session.kind);
        let session_branch = self
            .worktrees
            .iter()
            .find(|item| item.path == session.cwd)
            .and_then(|item| item.branch.clone())
            .unwrap_or_else(|| hunk.theirs_label.clone());

        let rem_px = self.effective_code_rem_px();
        let (ours_rendered, theirs_rendered): (
            &[code_view::RenderedLine],
            &[code_view::RenderedLine],
        ) = self
            .merge_highlight_cache
            .as_ref()
            .filter(|(cached_path, cached_hunk, _, _)| {
                cached_path == relative_path && cached_hunk == hunk
            })
            .map(|(_, _, ours, theirs)| (ours.as_slice(), theirs.as_slice()))
            .unwrap_or((&[], &[]));

        let column = |side: &'static str,
                      label: String,
                      sub: String,
                      raw_lines: &[String],
                      rendered_lines: &[code_view::RenderedLine],
                      start_line: usize,
                      take_id: &'static str,
                      take_label: &'static str,
                      choice: wt_core::merge::ConflictChoice,
                      rem_px: f32,
                      cx: &mut Context<Self>| {
            let code_rows = div()
                .flex()
                .flex_col()
                .size_full()
                .p(px(10.0))
                .font(font(theme::font::MONO))
                .children(raw_lines.iter().enumerate().map(|(index, raw_line)| {
                    let line_number = start_line + index;
                    let mut row = div()
                        .flex()
                        .items_center()
                        .text_size(rems(1.0))
                        .line_height(rems(1.6))
                        .debug_selector(move || format!("merge-{side}-code-row-{line_number}"));
                    row = row.child(
                        div()
                            .flex_none()
                            // Widened from an earlier px(28.0) to px(44.0), plus a real
                            // whitespace/overflow backstop - a real conflicted file can be
                            // thousands of real lines long, and a wrapped 5-digit gutter number
                            // would grow this row past its neighbours', the same class of bug
                            // Revision R5's audit fixed once already for the File view's own
                            // gutter (see `code_surface::diff_view::render_diff_gutter_number`'s matching
                            // fix and docs).
                            .w(px(44.0))
                            .pr(px(6.0))
                            .text_right()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(10.0))
                            .text_color(theme::text::GUTTER)
                            .child(line_number.to_string()),
                    );
                    // `rendered_lines.get(index)` - not indexing `rendered_lines` unconditionally
                    // - so a still-stale/mismatched cache falls back to `raw_line`'s real,
                    // plainly-coloured text rather than panicking or silently dropping the row
                    // (mirrors `code_surface::diff_view::render_diff_line`'s identical defensive fallback).
                    let mut text_row = div().flex().flex_1().min_w_0();
                    match rendered_lines.get(index) {
                        Some(line) if !line.text.is_empty() => {
                            for (run_text, kind) in &line.runs {
                                text_row = text_row.child(
                                    div()
                                        .text_color(code_view::color_for_kind(*kind))
                                        .child(run_text.clone()),
                                );
                            }
                        }
                        Some(_) => text_row = text_row.child("\u{a0}"),
                        None => {
                            text_row = text_row.child(div().text_color(theme::syntax::TEXT).child(
                                if raw_line.is_empty() {
                                    "\u{a0}".to_string()
                                } else {
                                    raw_line.clone()
                                },
                            ))
                        }
                    }
                    row.child(text_row)
                }));

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
                        .child(zoom_scoped(rem_px, code_rows)),
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
                        "ours",
                        base_branch.to_string(),
                        hunk.ours_label.clone(),
                        &hunk.ours,
                        ours_rendered,
                        hunk.ours_start_line,
                        "take-left",
                        "Take left",
                        wt_core::merge::ConflictChoice::Left,
                        rem_px,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .child(
                        // The right column's real, deliberate legibility fix - a 3px left-edge
                        // accent bar (matching `render_diff_line`'s own accent-bar precedent for
                        // the Diff view's add/remove signal) instead of a full-height wash
                        // sitting directly under real syntax-colored text. See this method's own
                        // `agent_fg` binding for the real contrast numbers this replaces.
                        div().flex_none().w(px(3.0)).self_stretch().bg(agent_fg),
                    )
                    .child(column(
                        "theirs",
                        session.kind.label().to_string(),
                        session_branch,
                        &hunk.theirs,
                        theirs_rendered,
                        hunk.theirs_start_line,
                        "take-right",
                        "Take right",
                        wt_core::merge::ConflictChoice::Right,
                        rem_px,
                        cx,
                    )),
            )
    }

    /// The `Take both` action on the active hunk - `wt_core::merge::ConflictChoice::Both` keeps
    /// both sides' lines (ours then theirs), via the same function
    /// [`Self::render_conflict_columns`]'s Take-left/Take-right buttons call.
    pub(in crate::merge) fn render_take_both_row(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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

    /// The real, discoverable toggle (Revision R8.5c) into the merge hand-edit whole-file editor
    /// (`crate::merge::editing::AdeApp::render_merge_edit_view`) for the currently active
    /// conflicted file - see `Self::render_merge_flow_surface`'s own docs for why this row is
    /// structurally absent (not merely disabled) once hand-edit mode is actually on.
    pub(in crate::merge) fn render_hand_edit_toggle_row(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .py(px(6.0))
            .bg(theme::surface::FOOTER)
            .child(
                div()
                    .id("merge-hand-edit-toggle")
                    .cursor_pointer()
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::text::FAINT)
                    .hover(|el| el.text_color(theme::text::SECONDARY))
                    .child("Edit conflict markers by hand")
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.start_merge_hand_edit(window, cx);
                    })),
            )
    }

    /// The distinct panel for [`wt_core::merge::ConflictedPath::Unmergeable`] entries - modify/
    /// delete or binary conflicts with no text-hunk resolution action (see that type's docs).
    /// Never rendered as resolved or as the normal two-column editor, since there's no hunk to
    /// show either way - lists each path and reason, and points at a terminal to resolve by
    /// hand.
    pub(in crate::merge) fn render_unmergeable_panel(
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

    /// Surface D's footer: `Complete merge` (`git commit`, enabled once `resolved`) and
    /// `Abort merge` (`git merge --abort`, always available) - see
    /// [`Self::complete_merge_flow`]/[`Self::abort_merge_flow`]'s docs. `in_flight`
    /// (`Self::merge_op_in_flight`) dims and disables both while a previous click's background
    /// operation is still running, defense in depth alongside the guard clause each of those
    /// methods already has.
    pub(in crate::merge) fn render_merge_flow_footer(
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

    /// A simple message panel (`AlreadyUpToDate`/`Error` states) - a title, the message text, an
    /// `Abort merge` action when `abortable_worktree` is `Some` (a merge is still in progress -
    /// see `merge::MergeFlowState::Error`'s docs), and a `Dismiss` action that clears
    /// [`Self::merge_flow`] without touching git.
    pub(in crate::merge) fn render_merge_message(
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
}
