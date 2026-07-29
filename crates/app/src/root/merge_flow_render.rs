use super::*;

impl AdeApp {
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
}
