//! The real GPUI surface for the agent review tab (GitHub issue #225): its tab strip entry, its
//! header/body/footer, and the `impl AdeApp` glue that opens, closes and focuses it. See `super`'s
//! module docs for scope.
//!
//! ## Open/close/focus discipline
//!
//! [`AdeApp::open_review_tab`]/[`AdeApp::leave_review_tab`]/[`AdeApp::close_review_tab`] are
//! deliberate copies of `crate::graph_view::render::AdeApp::open_git_graph`/`leave_graph_tab`/
//! `close_git_graph_tab`, step for step. That file's own comments document real, live-reproduced
//! dangling-focus bugs this app has already hit twice (a handle that stops being `track_focus`'d
//! the moment its tab stops rendering, while `Window::focus` still points at it, and an
//! `OverlayFocus` still holding it as a restore target). [`Self::review_focus_handle`] has exactly
//! the same conditional-render lifetime, so it needs exactly the same sweep - not a shortened
//! version of it because a step looked unnecessary.

use super::state::{
    review_empty_message, review_summary_label, review_tab_header, ReviewLoadState,
};
use super::*;

use crate::code_surface::diff_view::DiffDetailSurface;
use crate::root::widgets::{render_sidebar_message, render_status_letter};
use crate::work_surface::render::{DraggedTab, TabChromeArgs};

impl AdeApp {
    /// Opens (or re-activates) the review tab for agent `id`. The single real door: the footer's
    /// `Review` action and the tab's own click handler both come through here.
    ///
    /// Refuses outright when [`Self::review_available_for`] says no - no baseline captured yet, or
    /// a multi-agent worktree (GitHub issue #225's single-agent gate). A refusal is a genuine
    /// no-op: the surface simply doesn't exist for that agent right now, and every entry point
    /// that could reach it is rendered disabled to match, so this is a backstop rather than the
    /// primary gate.
    pub(crate) fn open_review_tab(
        &mut self,
        id: AgentId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.review_available_for(id) {
            return;
        }
        // Mirrors `open_git_graph`'s own defensive close: this is reachable from the footer while
        // the palette also happens to be open.
        if self.palette_open {
            self.close_palette(window, cx);
        }

        // The review tab and the graph tab both occupy the centre pane, so opening one must leave
        // the other properly - not merely stop drawing it, which is what left `graph_focus_handle`
        // dangling in the bugs `leave_graph_tab` documents.
        self.leave_graph_tab(window, cx);
        self.graph_tab_open = false;
        // GitHub issue #227: the run-transcript tab occupies the centre pane exactly as the
        // graph and review tabs do, so it needs the identical teardown - see
        // `crate::run_history::tab::AdeApp::leave_run_tab`.
        self.leave_run_tab(window, cx);

        let was_active = self.review_tab_active && self.review_tab_open == Some(id);
        self.review_tab_open = Some(id);
        self.review_tab_active = true;
        self.open_change = None;
        self.plus_menu_open = false;
        self.title_menu_open = None;
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;

        // Same "leaving Files"/"leaving the code surface" sweep `open_git_graph` performs, for the
        // identical reason: `self.open_change = None` above just unrendered the code surface, and
        // this tab replaces the centre pane the file tree's own focus interacts with. See
        // `crate::sidebar::render::AdeApp::set_right_sidebar_view`'s docs for the bug class.
        self.tree_context_menu = None;
        self.tree_inline_edit = None;
        if self.tree_focus_handle.is_focused(window) {
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
        }
        self.palette_focus.forget_target(&self.tree_focus_handle);
        self.settings_focus.forget_target(&self.tree_focus_handle);
        self.code_focus.forget_target(&self.tree_focus_handle);

        if self.code_focus_handle.is_focused(window) {
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.code_focus, fallback, window, cx);
        }
        self.palette_focus.forget_target(&self.code_focus_handle);
        self.settings_focus.forget_target(&self.code_focus_handle);
        // `open_change` just changed; every cache keyed on it must follow, exactly like every
        // other site that clears it.
        self.refresh_open_diff_file_cache();

        if !was_active && !self.focus_is_on_an_overlay(window, cx) {
            self.review_focus.capture(window, &self.agents, cx);
        }
        window.focus(&self.review_focus_handle, cx);

        if matches!(
            self.agent_reviews.get(&id).map(|review| &review.load),
            Some(ReviewLoadState::NotLoaded)
        ) {
            self.load_agent_review(id, cx);
        }
        cx.notify();
    }

    /// Closes the review tab outright (its `×`), removing it from the tab strip. Unlike the graph
    /// tab this does **not** drop the loaded review: the baseline is unchanged, so what was loaded
    /// is still exactly the right answer, and re-opening should not have to re-run `git diff`.
    /// (Closing the *agent* is what really discards a review - see
    /// `crate::review::flow::AdeApp::release_review_baseline`.)
    pub(crate) fn close_review_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_review_tab(window, cx);
        self.review_tab_open = None;
        cx.notify();
    }

    /// Closes the review tab if the single-agent gate has just shut on it - i.e. a second agent
    /// started in the worktree whose agent the tab is reviewing.
    ///
    /// Called right after every spawn. This is a real close, with the full focus teardown, rather
    /// than merely filtering the tab out of the strip: `review_focus_handle` stops being
    /// `track_focus`'d the moment the tab stops rendering, so quietly dropping it from the strip
    /// while `Window::focus` still pointed at it is precisely the dangling-focus bug class this
    /// module's docs describe.
    ///
    /// The agent's *review* survives (`agent_reviews` keeps its baseline), so closing the extra
    /// agent later and re-opening the tab picks up exactly where this left off - against the
    /// baseline captured at spawn, not a retroactive one.
    pub(crate) fn close_gated_review_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.review_tab_open {
            if !self.review_available_for(id) {
                self.close_review_tab(window, cx);
            }
        }
    }

    /// Common bookkeeping whenever the review tab stops being the active centre-pane content -
    /// selecting an agent/file/graph tab while it was showing, or closing it outright. A no-op if
    /// it wasn't active.
    ///
    /// [`Self::review_focus_handle`] is about to stop being `track_focus`'d
    /// ([`Self::render_review_view`] is only rendered while `review_tab_active`), so real keyboard
    /// focus moves off it *first*, before anything else can capture it as an `OverlayFocus` return
    /// target - and any target already holding it from earlier is swept. See this module's docs.
    pub(crate) fn leave_review_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.review_tab_active {
            return;
        }
        self.review_tab_active = false;
        if self.review_focus_handle.is_focused(window) {
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.review_focus, fallback, window, cx);
        }
        self.palette_focus.forget_target(&self.review_focus_handle);
        self.settings_focus.forget_target(&self.review_focus_handle);
        self.code_focus.forget_target(&self.review_focus_handle);
        self.graph_focus.forget_target(&self.review_focus_handle);
    }

    /// Opens `path`'s hunks inside the review tab - the review-side counterpart to
    /// `Self::open_change_diff`, deliberately *not* that function: opening a git Diff view tab
    /// from a review row would drop the user straight back into the surface this feature exists
    /// to keep separate, showing the same file measured against a different base.
    pub(crate) fn open_review_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let Some(id) = self.review_tab_open else {
            return;
        };
        let Some(review) = self.agent_reviews.get_mut(&id) else {
            return;
        };
        // Clicking the already-open row closes it again, so the file list is always reachable
        // without needing a separate "back" affordance.
        review.open_file = if review.open_file.as_deref() == Some(path.as_path()) {
            None
        } else {
            Some(path)
        };
        self.refresh_review_highlight_cache();
        cx.notify();
    }

    /// Rebuilds [`Self::review_highlight_cache`] for whichever file the review tab currently has
    /// open - the review-side twin of [`Self::ensure_diff_highlight_cache`], with the identical
    /// "recompute only when the cached `DiffFile` actually differs" freshness check, and called
    /// from the same kind of place: real change points (a row click, a completed load, a
    /// baseline advance), never from inside `render()`.
    pub(crate) fn refresh_review_highlight_cache(&mut self) {
        let Some(file) = self.open_review_file_detail().cloned() else {
            self.review_highlight_cache = None;
            return;
        };
        if self
            .review_highlight_cache
            .as_ref()
            .is_some_and(|(cached, _, _)| cached == &file)
        {
            return;
        }
        let extension = file.path.extension().and_then(|ext| ext.to_str());
        let highlight_options = self.highlight_options();
        let mut remaining = MAX_RENDERED_DIFF_LINES_PER_FILE;
        let mut per_hunk = Vec::with_capacity(file.hunks.len());
        let mut per_hunk_numbers = Vec::with_capacity(file.hunks.len());
        for hunk in &file.hunks {
            if remaining == 0 {
                break;
            }
            let capped_lines: Vec<&str> = hunk
                .lines
                .iter()
                .take(remaining)
                .map(|line| line.content.as_str())
                .collect();
            remaining -= capped_lines.len();
            per_hunk.push(code_view::highlight_block(
                capped_lines,
                extension,
                highlight_options,
            ));
            per_hunk_numbers.push(changes::hunk_line_numbers(hunk));
        }
        self.review_highlight_cache = Some((file, per_hunk, per_hunk_numbers));
    }

    /// The `DiffFile` the review tab currently has open, if any - a real lookup into the loaded
    /// review, never a cached copy that could go stale against it.
    ///
    /// `pub(crate)`, not private: `crate::code_surface::diff_view::DiffDetailSurface::open_file`
    /// calls this directly so the virtualized `uniform_list` row builder (GitHub issue #224) can
    /// re-resolve the *review* tab's own open file the same way it already re-resolves the git
    /// Diff view's (`AdeApp::open_diff_file_cache`) - one accessor per surface, not a shared one.
    pub(crate) fn open_review_file_detail(&self) -> Option<&wt_core::diff::DiffFile> {
        let id = self.review_tab_open?;
        let review = self.agent_reviews.get(&id)?;
        let open = review.open_file.as_deref()?;
        review.diff()?.files.iter().find(|file| file.path == open)
    }

    /// The label the review tab and its header show for agent `id` - the agent's real program
    /// label (`claude`, `codex`, the resolved shell), never a hardcoded name.
    fn review_agent_label(&self, id: AgentId, cx: &App) -> String {
        self.agents
            .iter()
            .find(|agent| agent.id == id)
            .map(|agent| agent.pane.read(cx).program_label())
            .unwrap_or_else(|| "agent".to_string())
    }

    /// The review tab's whole body: the header stating what this is measured against, the list of
    /// unreviewed files, the open file's hunks, and the `Mark reviewed` footer.
    pub(crate) fn render_review_view(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(id) = self.review_tab_open else {
            return div().into_any_element();
        };
        // The gate is re-checked at render time, not just at open time: a *second* agent spawning
        // into this worktree while the tab is open must make the surface stop claiming to show
        // "this agent's" changes immediately, without waiting for a click.
        if !self.review_available_for(id) {
            return div()
                .id("review-view")
                .track_focus(&self.review_focus_handle.clone())
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .bg(theme::surface::PTY)
                .child(render_sidebar_message(
                    "review is unavailable while more than one agent is running in this \
                     worktree - close the others to review this agent's changes on their own"
                        .to_string(),
                    theme::text::FAINT.into(),
                ))
                .into_any_element();
        }

        let agent_label = self.review_agent_label(id, cx);
        let Some(review) = self.agent_reviews.get(&id) else {
            return div().into_any_element();
        };
        let header_text = review_tab_header(&agent_label, &review.baseline);
        let summary = review_summary_label(review);
        let reason = review.baseline.reason;
        let load_is_error = matches!(review.load, ReviewLoadState::Error(_));
        let error_text = match &review.load {
            ReviewLoadState::Error(message) => Some(message.clone()),
            _ => None,
        };
        let files: Vec<wt_core::diff::DiffFile> = review
            .diff()
            .map(|diff| diff.files.clone())
            .unwrap_or_default();
        let open_file = review.open_file.clone();
        let truncated = review.diff().is_some_and(|diff| diff.truncated);

        let mut body = div()
            .id("review-view")
            .track_focus(&self.review_focus_handle.clone())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(theme::surface::PTY)
            .child(self.render_review_header(header_text, summary, cx));

        if let Some(message) = error_text {
            body = body.child(render_sidebar_message(
                format!("could not load this review: {message}"),
                theme::status::FAIL.into(),
            ));
        } else if files.is_empty() && !load_is_error {
            // The good empty state - deliberately worded as success, and deliberately different
            // from the git side's own "this branch matches main". See `state::review_empty_message`.
            body = body.child(render_sidebar_message(
                review_empty_message(reason).to_string(),
                theme::text::FAINT.into(),
            ));
        } else {
            let mut list = div().flex().flex_col().flex_none();
            for file in &files {
                list = list.child(self.render_review_file_row(file, open_file.as_deref(), cx));
            }
            if truncated {
                list = list.child(render_sidebar_message(
                    "\u{2026} this review was too large to show in full".to_string(),
                    theme::text::FAINT.into(),
                ));
            }
            body = body.child(list);

            if let Some(file) = self.open_review_file_detail().cloned() {
                // The one real diff renderer in this app, reused verbatim against this review's
                // own `WorktreeDiff` - see `DiffDetailSurface`'s own docs.
                body =
                    body.child(self.render_diff_file_detail(&file, DiffDetailSurface::Review, cx));
            } else {
                body = body.child(div().flex_1().min_h_0());
            }
        }

        body.child(self.render_review_footer(id, cx))
            .into_any_element()
    }

    /// The header band: what this review is measured against and when
    /// (`state::review_tab_header`), plus how much is unreviewed.
    fn render_review_header(
        &self,
        header_text: String,
        summary: String,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .h(theme::band::CHROME_HEADER)
            .bg(theme::surface::FOOTER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .debug_selector(|| "review-header".to_string())
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(11.0))
                    .text_color(theme::text::PRIMARY)
                    .child(header_text),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::DIMMER)
                    .child(summary),
            )
    }

    /// One unreviewed file row. Deliberately its own row rather than
    /// `crate::sidebar::render::AdeApp::render_change_row`: that row carries a **staging**
    /// checkbox and a `committed` marker, both of which are git-side concepts about the index and
    /// about commits on this branch. Neither means anything about a review, and showing them here
    /// would re-merge exactly the two concepts this feature exists to separate.
    fn render_review_file_row(
        &self,
        file: &wt_core::diff::DiffFile,
        open_file: Option<&Path>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let path = file.path.clone();
        let selected = open_file == Some(file.path.as_path());
        let (add, del) = changes::diff_file_stats(file);
        let (dir, name) = changes::split_dir_name(&file.path);
        let letter = changes::status_letter(file.status);

        div()
            .id(format!("review-row-{}", file.path.display()))
            .debug_selector(|| format!("review-row-{}", file.path.display()))
            .relative()
            .flex()
            .w_full()
            .items_center()
            .gap(px(6.0))
            .h(theme::band::CHANGE_ROW)
            .px(px(10.0))
            .cursor_pointer()
            // No bottom border at all - see `crate::graph_view::render::AdeApp::render_graph_row`'s
            // identical fix for why: GPUI's `Style::border_color` is one shared value for the
            // whole element, so a conditional `border_l_2()` here used to silently recolour a
            // permanent `border_b_1()` separator too, a real border appearing along the bottom on
            // selection - and it only reserved its own space while selected, shifting every row's
            // content on click. Fixed the same way: no bottom border, and a real, separate,
            // always-painted child for the left selection edge.
            .child(
                div()
                    .debug_selector({
                        let path = file.path.clone();
                        move || format!("review-row-{}-selection-edge", path.display())
                    })
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(if selected {
                        theme::border::SELECTED_EDGE.into()
                    } else {
                        work_surface::TRANSPARENT
                    }),
            )
            .when(selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.open_review_file(path.clone(), cx);
            }))
            // `STAGE-A-CHANGELOG.md` §4j's fixed column, ahead of the directory - this is a
            // list of file rows like the Uncommitted section, so every filename in it starts on
            // the same x. The `new`/`del` word pill this replaced sat at the far end of the row
            // and was absent from every modified file.
            .child(render_status_letter(
                gpui::SharedString::from(format!("review-status-{}", file.path.display())),
                letter,
                self.ui_text_size(10.0),
            ))
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
                    .text_color(theme::text::STRONG)
                    .child(name),
            )
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
    }

    /// The review tab's footer: the real `Mark reviewed` action.
    ///
    /// Disabled - dimmed, with no `cursor_pointer`/`on_click` at all, this crate's established
    /// convention - while a mark is already in flight, and when there is nothing unreviewed to
    /// mark. The second case matters: clicking it with an empty review would take a fresh
    /// snapshot, move the baseline's timestamp, and change the header's "since" time for no
    /// reason the user asked for.
    fn render_review_footer(&self, id: AgentId, cx: &mut Context<Self>) -> impl IntoElement {
        let in_flight = self.review_mark_in_flight == Some(id);
        let has_changes = self
            .agent_reviews
            .get(&id)
            .is_some_and(|review| review.has_unreviewed_changes());
        let enabled = has_changes && !in_flight;
        let label = if in_flight {
            "marking reviewed\u{2026}"
        } else {
            "Mark reviewed"
        };
        let colors = work_surface::action_button_colors(work_surface::ActionStyle::PrimaryGreen);

        let mut button = div()
            .id("review-mark-reviewed")
            .debug_selector(|| "review-mark-reviewed".to_string())
            .h(px(23.0))
            .px(px(10.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .bg(if enabled {
                colors.bg
            } else {
                work_surface::TRANSPARENT
            })
            .border_1()
            .border_color(if enabled {
                colors.border
            } else {
                theme::border::BUTTON_DISABLED.into()
            })
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        colors.fg
                    } else {
                        theme::text::GHOSTER.into()
                    })
                    .child(label),
            );
        if enabled {
            button = button
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.mark_reviewed(id, cx);
                }));
        } else {
            button = button.cursor_default();
        }

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .px(px(12.0))
            .h(theme::band::SURFACE_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER)
            .child(button)
            .child(div().flex_1())
    }
}

/// The tab strip's own review entry, rendered only for `TabRef::Review` - which
/// `work_surface::state::reconcile_tab_order` only ever produces while the tab is really open for
/// an agent of this worktree.
pub(crate) fn render_review_tab(
    app: &AdeApp,
    id: AgentId,
    cx: &mut Context<AdeApp>,
) -> gpui::AnyElement {
    let is_active = app.review_tab_active;
    let colors = work_surface::tab_colors(is_active);
    let close_color = if is_active {
        theme::text::DIMMER
    } else {
        theme::text::DISABLED
    };
    // Says "Review", never "Diff" - see `super`'s module docs on the vocabulary split.
    let label = "Review".to_string();

    let close_button = app.render_tab_close_button(
        "close-review-tab",
        close_color,
        None,
        |this, window, cx| {
            this.close_review_tab(window, cx);
        },
        cx,
    );
    let content: Vec<gpui::AnyElement> = vec![
        render_review_tab_chip().into_any_element(),
        div()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(app.ui_text_size(11.0))
            .text_color(colors.label)
            .child(label.clone())
            .into_any_element(),
        close_button.into_any_element(),
    ];

    app.render_tab_chrome(
        TabChromeArgs {
            outer_id: "review-tab".into(),
            hit_id: "review-tab-hit".into(),
            tab_ref: work_surface::TabRef::Review(id),
            drag_value: DraggedTab::Review { id, label },
            is_active,
            content,
            on_middle_click: Box::new(|this, window, cx| {
                this.close_review_tab(window, cx);
            }),
            on_activate: Box::new(move |this, window, cx| {
                this.open_review_tab(id, window, cx);
            }),
            debug_selector: Some("review-tab"),
        },
        cx,
    )
}

/// The review tab's own chip: a check glyph drawn from two rects, matching
/// `crate::graph_view::render::render_graph_tab_chip`'s "no icon font" approach (design spec §1).
pub(crate) fn render_review_tab_chip() -> impl IntoElement {
    div()
        .flex_none()
        .relative()
        .w(px(14.0))
        .h(px(14.0))
        .rounded(theme::radius::CHIP)
        .bg(theme::status::REVIEW_BG)
        .child(
            // The check's short, downward stroke.
            div()
                .absolute()
                .w(px(1.0))
                .h(px(4.0))
                .top(px(6.0))
                .left(px(4.0))
                .bg(theme::status::REVIEW),
        )
        .child(
            // The long, upward stroke.
            div()
                .absolute()
                .w(px(1.0))
                .h(px(7.0))
                .top(px(3.0))
                .left(px(9.0))
                .bg(theme::status::REVIEW),
        )
        .child(
            // The joining foot, so the two strokes read as one glyph rather than two ticks.
            div()
                .absolute()
                .w(px(6.0))
                .h(px(1.0))
                .top(px(9.0))
                .left(px(4.0))
                .bg(theme::status::REVIEW),
        )
}

/// Real, end-to-end coverage for the agent review surface (GitHub issue #225): a real git repo,
/// real agents spawned into it, real `wt_core::review` snapshots, and the real single-agent gate.
///
/// These deliberately drive the same entry points the UI does (`AdeApp::new_agent`,
/// `open_review_tab`, `mark_reviewed`, `close_agent`) rather than poking state directly, so they
/// prove the wiring, not just the pure logic underneath it.
#[cfg(test)]
mod review_flow_tests {
    use super::*;
    use crate::review::state::BaselineReason;
    use crate::root::focus::palette_focus_tests;
    use crate::test_support::{temp_repo_with, TempRoot};
    use crate::work_surface::agents::{AgentKind, ProcessKind};
    use gpui::TestAppContext;
    use test_support::{git, git_output, git_try};

    /// A real repo whose branch has **already diverged from `main`**, with a real committed
    /// change on it. That divergence is the whole point: it gives the worktree a real, non-empty
    /// *git* diff that has nothing to do with any agent, which is exactly the state that used to
    /// make an agent falsely report "review ready".
    fn diverged_repo() -> TempRoot {
        temp_repo_with(|root| {
            test_support::seed_empty_repo_at(root);
            test_support::commit(root, "base.txt", "base\n", "initial");
            git(root, &["checkout", "-b", "feature"]);
            test_support::commit(
                root,
                "already_here.txt",
                "committed on feature\n",
                "work that predates any agent",
            );
        })
    }

    /// Every window starts with exactly one agent - a plain shell `AdeApp::new` spawns into the
    /// repo root (see `root::state`). A shell is never review-eligible (see [`ProcessKind`]'s
    /// docs: it has no turns, so `capture_review_baseline` never captures one for it), so it
    /// can't be what these tests review. This closes it and spawns a real `Claude` agent into
    /// the same worktree in its place - still the *sole* agent there (what the single-agent gate
    /// requires), and still rooted at the same `repo.path()` every caller already writes test
    /// files into. Waits for the replacement's baseline capture to land, so callers don't need
    /// their own `run_until_parked` just to get a usable id.
    fn sole_agent(app: &gpui::Entity<AdeApp>, cx: &mut gpui::VisualTestContext) -> AgentId {
        let startup = app.read_with(cx, |app, _| {
            let ids: Vec<AgentId> = app.agents.iter().map(|agent| agent.id).collect();
            assert_eq!(
                ids.len(),
                1,
                "a fresh test window must start with exactly one agent"
            );
            ids[0]
        });
        app.update_in(cx, |app, window, cx| {
            app.close_agent(startup, window, cx);
            app.new_agent(ProcessKind::claude(), window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let ids: Vec<AgentId> = app.agents.iter().map(|agent| agent.id).collect();
            assert_eq!(
                ids.len(),
                1,
                "closing the startup shell and spawning its replacement must still leave exactly \
                 one agent"
            );
            ids[0]
        })
    }

    /// Spawns an *additional* agent through the real `new_agent` entry point (the same door the
    /// `+` menu uses) and waits for its baseline capture to land.
    ///
    /// Callers that need a second agent **in the sole agent's own worktree** must pass a kind
    /// other than [`AgentKind::Claude`] (what [`sole_agent`] itself now spawns): a baseline key
    /// is `(worktree, kind, spawn second)`, so a second `Claude` agent spawned into the same
    /// worktree within the same second would share the sole agent's key and therefore its
    /// baseline ref (this crate's documented, accepted collision - see
    /// `super::state::baseline_key`). Sharing it would make one agent's close delete the other's
    /// ref, which is not what any of these tests mean to exercise.
    fn spawn_extra_agent(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        kind: AgentKind,
    ) -> AgentId {
        app.update_in(cx, |app, window, cx| {
            app.new_agent(ProcessKind::Agent(kind), window, cx)
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            app.agents.iter().last().expect("a spawned agent").id
        })
    }

    /// Spawning an agent must capture a genuinely real baseline - a hex tree id git can resolve,
    /// anchored under a real ref so `git gc` can't take it - **without disturbing one byte** of
    /// the worktree the agent is about to work in.
    ///
    /// Both halves in one test deliberately: they need the same real, mixed staged/unstaged/
    /// untracked fixture, and every GPUI test app on a real git repo arms two OS-level `notify`
    /// watchers (`start_file_tree_watch`/`start_worktree_watch`), which are a genuinely scarce
    /// per-user resource under a fully parallel `cargo test` - see
    /// `crate::sidebar::file_tree_watch::spawn_file_tree_watcher`'s own docs on the inotify
    /// instance budget and the real regression that already exhausted it once.
    #[gpui::test]
    fn spawning_an_agent_captures_a_real_anchored_baseline_without_disturbing_git(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        // A genuinely mixed state, present *before* the app (and so before the snapshot) opens.
        std::fs::write(repo.path().join("staged.txt"), "staged\n").expect("write");
        git(repo.path(), &["add", "staged.txt"]);
        std::fs::write(repo.path().join("base.txt"), "base\nedited\n").expect("write");
        std::fs::write(repo.path().join("untracked.txt"), "untracked\n").expect("write");
        let status_before = git_output(repo.path(), &["status", "--porcelain"]);
        let head_before = git_output(repo.path(), &["rev-parse", "HEAD"]);

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        assert_eq!(
            git_output(repo.path(), &["status", "--porcelain"]),
            status_before,
            "capturing a baseline must not change one byte of the worktree's real git state"
        );
        assert_eq!(git_output(repo.path(), &["rev-parse", "HEAD"]), head_before);
        assert!(git_output(repo.path(), &["stash", "list"]).is_empty());

        let (tree_id, ref_name, reason) = app.read_with(cx, |app, _| {
            let review = app
                .agent_reviews
                .get(&id)
                .expect("spawning an agent must capture a real review baseline");
            (
                review.baseline.tree_id.clone(),
                review.baseline.ref_name.clone(),
                review.baseline.reason,
            )
        });

        assert_eq!(reason, BaselineReason::Spawn);
        assert!(
            ref_name.starts_with(wt_core::review::REVIEW_REF_PREFIX),
            "a baseline must be anchored inside this app's own ref namespace - got {ref_name}"
        );
        assert_eq!(
            git_output(repo.path(), &["rev-parse", &ref_name]),
            tree_id,
            "the anchored ref must really resolve to the captured tree in the real repository"
        );
        assert_eq!(
            git_output(repo.path(), &["cat-file", "-t", &tree_id]),
            "tree",
            "and that object must really be a tree"
        );
    }

    /// **The heart of GitHub issue #225.** A freshly spawned agent has changed nothing, so its
    /// review is empty - even though the worktree it is sitting in has a large, real *git* diff
    /// against `main`. Before this, that git diff was what drove "review ready", so this agent
    /// would have claimed work it never did.
    #[gpui::test]
    fn a_fresh_agents_review_is_empty_even_though_the_git_diff_is_not(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let review = app.agent_reviews.get(&id).expect("a review");
            assert_eq!(
                review.diff().expect("a really loaded review").files.len(),
                0,
                "a fresh agent has changed nothing, so its review must be empty"
            );
            assert!(!app.agent_has_unreviewed_changes(id));
            assert_eq!(app.agent_review_file_count(id), Some(0));

            // The same worktree, at the same instant, really does have a non-empty git diff -
            // which is what makes the empty review above a genuine distinction rather than an
            // empty repository.
            let git_files = app
                .current_diff()
                .expect("a real git diff is loaded")
                .files
                .len();
            assert!(
                git_files > 0,
                "the git diff must be non-empty here, or this test proves nothing about the two \
                 answers differing"
            );
        });
    }

    /// The other half: work the agent really does after it starts must appear in its review, and
    /// must be countable per-agent for the rail.
    #[gpui::test]
    fn work_done_after_an_agent_starts_shows_up_in_its_review(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        // Exactly what an agent CLI does: write a file into its own worktree.
        std::fs::write(
            repo.path().join("written_by_the_agent.rs"),
            "fn main() {}\n",
        )
        .expect("write");

        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let review = app.agent_reviews.get(&id).expect("a review");
            let paths: Vec<PathBuf> = review
                .diff()
                .expect("loaded")
                .files
                .iter()
                .map(|file| file.path.clone())
                .collect();
            assert_eq!(
                paths,
                vec![PathBuf::from("written_by_the_agent.rs")],
                "only what the agent really did since it started - not the branch's own \
                 already-committed work"
            );
            assert!(app.agent_has_unreviewed_changes(id));
            assert_eq!(app.agent_review_file_count(id), Some(1));
        });
    }

    /// `Mark reviewed` must take a fresh snapshot, advance the baseline (including its reason and
    /// timestamp), re-anchor the same ref, and leave a genuinely empty review behind.
    #[gpui::test]
    fn marking_reviewed_advances_the_baseline_and_empties_the_review(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");

        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();

        let (before_tree, ref_name) = app.read_with(cx, |app, _| {
            let review = app.agent_reviews.get(&id).expect("a review");
            assert!(
                review.has_unreviewed_changes(),
                "precondition: something to review"
            );
            (
                review.baseline.tree_id.clone(),
                review.baseline.ref_name.clone(),
            )
        });

        app.update(cx, |app, cx| app.mark_reviewed(id, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let review = app.agent_reviews.get(&id).expect("a review");
            assert_eq!(review.baseline.reason, BaselineReason::MarkedReviewed);
            assert_ne!(
                review.baseline.tree_id, before_tree,
                "marking reviewed must move the baseline onto a genuinely new snapshot"
            );
            assert_eq!(
                review.baseline.ref_name, ref_name,
                "and must move the same ref rather than accumulating one per mark"
            );
            assert_eq!(
                review.diff().expect("reloaded").files.len(),
                0,
                "nothing has changed since a snapshot taken a moment ago - the good empty state"
            );
            assert!(!app.agent_has_unreviewed_changes(id));
            assert!(review.open_file.is_none());
            assert_eq!(
                git_output(repo.path(), &["rev-parse", &ref_name]),
                review.baseline.tree_id,
                "the real ref must really point at the new snapshot"
            );
        });

        // And further work after marking is unreviewed again - the baseline advanced, it didn't
        // stop tracking.
        std::fs::write(repo.path().join("more_work.rs"), "fn more() {}\n").expect("write");
        app.update(cx, |app, cx| app.load_agent_review(id, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.agent_review_file_count(id), Some(1));
        });
    }

    /// **The single-agent gate, proved in both directions.** A second agent opening in the same
    /// worktree must hide the whole review surface (no tab, no footer door, no review-ready
    /// status, no file count); closing it again must reveal it, against the baseline that was
    /// quietly captured all along.
    #[gpui::test]
    fn a_second_agent_hides_the_review_surface_and_closing_it_reveals_it_again(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let first = sole_agent(&app, cx);
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");

        app.update_in(cx, |app, window, cx| app.open_review_tab(first, window, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.review_available_for(first), "one agent: available");
            assert!(app.agent_has_unreviewed_changes(first));
            assert_eq!(app.agent_review_file_count(first), Some(1));
            assert_eq!(app.review_tab_open, Some(first));
            assert!(
                app.combined_tab_order()
                    .contains(&work_surface::TabRef::Review(first)),
                "the review tab must really be in the strip"
            );
        });

        // A second agent starts in the same worktree.
        // `Codex`, not `Claude`: the kind only has to *differ* from the sole agent's own (see
        // `spawn_extra_agent`) to get a distinct baseline key, and using the same kind twice
        // would additionally spawn a second real `claude` CLI - a heavy Node process whose load
        // was measurably breaking this crate's timing-sensitive inotify tests running in
        // parallel. Nothing this test asserts depends on which binary runs.
        let second = spawn_extra_agent(&app, cx, AgentKind::Codex);

        app.read_with(cx, |app, _| {
            assert!(
                !app.review_available_for(first),
                "with two agents sharing a worktree, this agent's review would include changes it \
                 did not make - the surface must be held back"
            );
            assert!(!app.review_available_for(second));
            assert!(
                !app.agent_has_unreviewed_changes(first),
                "and no agent may claim review-readiness it cannot substantiate"
            );
            assert_eq!(
                app.agent_review_file_count(first),
                None,
                "an honestly-absent count, never a fabricated one"
            );
            assert!(
                !app.combined_tab_order()
                    .contains(&work_surface::TabRef::Review(first)),
                "and the review tab must be gone from the strip"
            );
            // A baseline was still *captured* for both - the gate is display-time only, so the
            // surface can simply start working again once the worktree is back to one agent.
            assert!(app.agent_reviews.contains_key(&first));
            assert!(app.agent_reviews.contains_key(&second));
        });

        // And trying to open it while gated is a real no-op, not a tab that opens and then
        // renders an apology.
        app.update_in(cx, |app, window, cx| app.open_review_tab(first, window, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.review_tab_open, None,
                "gated: opening must refuse outright"
            );
            assert!(!app.review_tab_active);
        });

        // Close the second agent - back down to one.
        app.update_in(cx, |app, window, cx| app.close_agent(second, window, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.review_available_for(first),
                "back down to a single agent, the review surface must be available again"
            );
            assert!(app.agent_has_unreviewed_changes(first));
            assert_eq!(
                app.agent_review_file_count(first),
                Some(1),
                "against the baseline captured at spawn all along, not a retroactive one"
            );
        });

        // And it can really be re-opened.
        app.update_in(cx, |app, window, cx| app.open_review_tab(first, window, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.review_tab_open, Some(first));
            assert!(app
                .combined_tab_order()
                .contains(&work_surface::TabRef::Review(first)));
        });
    }

    /// **A plain terminal sharing the worktree does not gate anything** (GitHub issue #381).
    ///
    /// The single-agent gate exists so one agent's review can't claim another agent's changes;
    /// a [`ProcessKind::Shell`] is not a party it can ever be confused with - no baseline is
    /// captured for one, it has no turns, and it can never open a review surface of its own. It
    /// used to be counted anyway, and the consequence was not a corner case: `select_worktree`
    /// opens a startup shell in every worktree that has no tab yet, so the *first* agent started
    /// anywhere already shared its worktree with that shell and the gate never opened for it. The
    /// review surface was effectively dead in the default configuration - `crate::sound::flow`'s
    /// module docs describe exactly this and route around it.
    ///
    /// Deliberately does **not** use [`sole_agent`], whose whole job is to close the startup
    /// shell first: keeping that shell open is the entire point of this test.
    #[gpui::test]
    fn a_plain_shell_in_the_same_worktree_does_not_gate_the_review_surface(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let shell = app.read_with(cx, |app, _| {
            let ids: Vec<AgentId> = app.agents.iter().map(|agent| agent.id).collect();
            assert_eq!(ids.len(), 1, "the window's own startup shell");
            ids[0]
        });
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents
                    .iter()
                    .find(|agent| agent.id == shell)
                    .map(|agent| agent.kind),
                Some(ProcessKind::Shell),
                "sanity check: the startup tab really is a shell, not an agent session"
            );
        });

        // A real agent alongside it - the shell stays open, exactly as it does in the app.
        app.update_in(cx, |app, window, cx| {
            app.new_agent(ProcessKind::claude(), window, cx);
        });
        cx.run_until_parked();
        let agent = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("the agent just spawned")
        });
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");
        app.update(cx, |app, cx| app.load_agent_review(agent, cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                2,
                "sanity check: the shell and the agent really are both open in this worktree"
            );
            assert!(
                app.agents.is_sole_agent_in_worktree(agent),
                "a terminal is not an agent - the agent is still the only *agent session* here"
            );
            assert!(
                app.review_available_for(agent),
                "an open terminal must not hold back the whole review surface"
            );
            assert_eq!(app.agent_review_file_count(agent), Some(1));
            assert!(
                !app.agents.is_sole_agent_in_worktree(shell),
                "and the shell itself is never the subject of this gate - it has no baseline and \
                 nothing to review"
            );
            assert!(
                !app.review_available_for(shell),
                "a shell must never get a review surface of its own"
            );
        });

        // The tab really opens, not just the predicate.
        app.update_in(cx, |app, window, cx| app.open_review_tab(agent, window, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.review_tab_open, Some(agent));
            assert!(app
                .combined_tab_order()
                .contains(&work_surface::TabRef::Review(agent)));
        });
    }

    /// **Every real spawn door captures a baseline**, not just `new_agent`.
    ///
    /// Found by driving the running app while verifying GitHub issue #381: `new_agent_pane`
    /// (`ctrl-shift-N`, the title bar's `New Agent Pane` row, and the empty pane's own
    /// `Start an agent` CTA) and `respawn_agent` (`Retry`/`Resume`) both spawned a real agent and
    /// never captured one, so no agent started through them could ever open a review surface -
    /// and `new_agent_pane` is how most agents in this app are actually started. `respawn_agent`
    /// is the worse of the two: its own `close_agent` has already *released* the previous
    /// agent's ref, so a retried agent was left with neither.
    ///
    /// Asserted through `AdeApp::agent_reviews` (a real captured baseline with a real
    /// `refs/jerry/review/*` ref behind it), not merely through `review_available_for`, so this
    /// can't pass on the single-agent gate alone.
    #[gpui::test]
    fn every_spawn_door_captures_a_review_baseline(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // `new_agent_pane` resolves the first agent CLI really installed on `$PATH` in the
        // background before it spawns, so this needs a real parked run to land.
        app.update(cx, |app, cx| app.new_agent_pane(cx));
        cx.run_until_parked();
        let from_pane_door = app.read_with(cx, |app, _| {
            app.agents
                .active_id()
                .expect("new_agent_pane spawned a tab")
        });
        app.read_with(cx, |app, _| {
            assert!(
                app.agents
                    .iter()
                    .find(|agent| agent.id == from_pane_door)
                    .is_some_and(|agent| agent.kind.is_agent_session()),
                "sanity check: this door spawns a real agent session, not a shell"
            );
            let review = app
                .agent_reviews
                .get(&from_pane_door)
                .expect("`New Agent Pane` must capture a review baseline like every other door");
            assert!(
                !review.baseline.tree_id.is_empty(),
                "and a real snapshot behind it, not an empty placeholder"
            );
            assert_eq!(
                git_output(repo.path(), &["rev-parse", &review.baseline.ref_name]),
                review.baseline.tree_id,
                "the real ref must point at that snapshot"
            );
        });

        // `Retry`/`Resume`: closes the tab (releasing its ref) and spawns a fresh agent.
        app.update_in(cx, |app, window, cx| {
            app.respawn_agent(from_pane_door, window, cx)
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let respawned = app.agents.active_id().expect("respawn_agent spawned a tab");
            assert_ne!(
                respawned, from_pane_door,
                "sanity check: a respawn is a genuinely new agent, not the old one revived"
            );
            let review = app.agent_reviews.get(&respawned).expect(
                "a retried agent must get its own baseline - its predecessor's ref was just \
                 released, so without one it has nothing at all to review against",
            );
            assert_eq!(
                git_output(repo.path(), &["rev-parse", &review.baseline.ref_name]),
                review.baseline.tree_id
            );
        });
    }

    /// Closing an agent releases its baseline ref (so `git gc` can reclaim the objects) but
    /// deliberately keeps the persisted metadata entry - the groundwork GitHub issue #227 needs.
    #[gpui::test]
    fn closing_an_agent_releases_its_ref_but_keeps_the_persisted_record(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        let ref_name = app.read_with(cx, |app, _| {
            app.agent_reviews
                .get(&id)
                .expect("a review")
                .baseline
                .ref_name
                .clone()
        });
        assert!(!git_output(repo.path(), &["rev-parse", &ref_name]).is_empty());

        app.update_in(cx, |app, window, cx| app.close_agent(id, window, cx));
        cx.run_until_parked();

        assert!(
            git_try(repo.path(), &["rev-parse", "--verify", &ref_name])
                .stdout
                .is_empty(),
            "a closed agent's baseline ref must really be deleted"
        );
        app.read_with(cx, |app, _| {
            assert!(
                !app.agent_reviews.contains_key(&id),
                "and its in-memory review must be gone"
            );
            assert!(
                app.review_baseline_state
                    .baselines
                    .values()
                    .any(|entry| entry.ref_name == ref_name),
                "but the persisted record of what was captured must survive - it is exactly the \
                 data GitHub issue #227 will need, and this app must not destroy it"
            );
        });
    }

    /// The header is the thing that actually resolves the issue's "confusion" complaint, so it
    /// must be built from the real baseline and must really paint.
    #[gpui::test]
    fn the_review_tab_header_states_the_real_since_point_and_really_paints(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");

        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("review-header").is_some(),
            "the review tab's header must really have painted"
        );
        assert!(
            cx.debug_bounds("review-row-agent_work.rs").is_some(),
            "and the real unreviewed file must really have painted as a row"
        );

        let header = app.read_with(cx, |app, cx| {
            let review = app.agent_reviews.get(&id).expect("a review");
            crate::review::state::review_tab_header(
                &app.review_agent_label(id, cx),
                &review.baseline,
            )
        });
        assert!(
            header.contains("since it started"),
            "a spawn baseline must say what it is measured against - got {header:?}"
        );
        assert!(
            !header.to_lowercase().contains("diff"),
            "and must never use the git side's own word - got {header:?}"
        );

        // Clicking the row really opens that file's hunks, through the review's own renderer.
        app.update(cx, |app, cx| {
            app.open_review_file(PathBuf::from("agent_work.rs"), cx)
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("review-line-0").is_some(),
            "the review's own diff rows must really paint, under their own selector prefix"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.review_highlight_cache.is_some(),
                "and the review's own highlight cache must be populated - not the git Diff \
                 view's, which stays independent"
            );
        });
    }

    /// The reported "double border left and bottom" applied to the review panel's own rows -
    /// see `crate::graph_view::render::AdeApp::render_graph_row`'s own docs for the full GPUI
    /// `Style::border_color`-is-one-shared-value explanation this fix is built on.
    #[gpui::test]
    fn the_selection_edge_is_a_real_element_painted_regardless_of_selection(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");

        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("review-row-agent_work.rs").is_some(),
            "sanity check: the real unreviewed file must really have painted as a row"
        );
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change, None,
                "premise: nothing is selected yet, so this genuinely exercises the unselected \
                 case"
            );
        });

        let edge_unselected = cx
            .debug_bounds("review-row-agent_work.rs-selection-edge")
            .expect(
                "the selection-edge child must be painted even while the row is unselected - if \
                 it's `None` here, the edge is still only created `.when(selected, ...)`, the \
                 exact regression this test exists to catch",
            );

        app.update(cx, |app, cx| {
            app.open_review_file(PathBuf::from("agent_work.rs"), cx)
        });
        cx.run_until_parked();

        let edge_selected = cx
            .debug_bounds("review-row-agent_work.rs-selection-edge")
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

    /// **The reachability proof.** Every other test in this module opens the review tab by
    /// calling `open_review_tab` directly, which proves the surface works but says nothing about
    /// whether a real user can ever get to it. This one touches none of the review API: it lets
    /// the rail's own status poll run, and checks that the real chain closes.
    ///
    /// That chain is genuinely circular-looking and was briefly broken during this build: the
    /// footer's `Review` door only appears on a `Status::Review` agent, `Status::Review` requires
    /// `agent_has_unreviewed_changes`, and if that had been derived from the tab's own loaded diff
    /// it could only ever become true *after* the tab was already open. The status poll's cheap
    /// `changed_paths_against_tree` measurement is what breaks the cycle - so this test is the one
    /// that would fail if that measurement were ever moved back inside the tab.
    #[gpui::test]
    fn the_status_poll_alone_makes_an_agent_really_reviewable(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        // Real agent work, and nothing else - no review API is touched anywhere in this test.
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");
        app.read_with(cx, |app, _| {
            assert!(
                !app.agent_has_unreviewed_changes(id),
                "precondition: nothing measured yet, so nothing may be claimed yet"
            );
            assert_eq!(app.agent_review_file_count(id), None);
        });

        // Let one real status-poll tick run.
        cx.background_executor
            .advance_clock(crate::root::STATUS_POLL_INTERVAL * 2);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.agent_has_unreviewed_changes(id),
                "the status poll alone must make an agent's real work reviewable - without it, \
                 `Status::Review` could never fire, so the review door could never appear, so \
                 the tab could never be opened at all"
            );
            assert_eq!(
                app.agent_review_file_count(id),
                Some(1),
                "and the rail's per-agent count must be real without the tab ever being opened"
            );
            assert!(
                app.review_available_for(id),
                "so the door is genuinely offerable"
            );
            // GitHub issue #295 moved that door from the finished agent's pane footer (§4r
            // emptied it entirely) to the title bar's Agent menu. Same gate, new home - and it
            // must really be enabled, not merely listed.
            assert!(
                app.menu_command_enabled(crate::title_bar::menu_model::MenuCommand::ReviewAgent),
                "the Agent menu's `Review Agent` row is the review tab's only door now, so it \
                 must be enabled exactly when `review_available_for` is true"
            );
        });

        // And that measurement is exactly the fact `derive_status` needs to reach
        // `Status::Review` - the status whose footer carries the real `Review` door. (The agent
        // here is a live `claude` process, so its *current* status is `Run`/`Ask`; what this pins
        // is that the input is now genuinely true, which is the half the poll is responsible
        // for. `Status::Review` is real-agent-only - see `AgentKind::is_agent_session`'s docs -
        // so this must use the sole agent's own real kind, not a placeholder.)
        let has_unreviewed = app.read_with(cx, |app, _| app.agent_has_unreviewed_changes(id));
        let status_on_exit = crate::rail::status::derive_status(
            ProcessKind::claude(),
            crate::rail::status::ProcessSignal::Exited { success: true },
            // An exit is an exact fact; no terminal-title/OSC signal takes part in it (see
            // `crate::rail::status`'s module docs), so the default "said nothing" is right here.
            crate::rail::status::TerminalSignal::default(),
            // Likewise no hook signal: this pins the *exit* path to `Review`, which is the one
            // that existed before GitHub issue #239 phase 2 and must keep working unchanged.
            crate::rail::status::HookSignal::default(),
            has_unreviewed,
        );
        assert_eq!(
            status_on_exit,
            crate::rail::status::Status::Review,
            "a successfully-exited agent with real unreviewed work must reach Status::Review"
        );
        // GitHub issue #295 / §4r: that status's footer must now carry **nothing at all** - "a
        // finished transcript is a record; its actions live where their object lives". The door
        // asserted above (the Agent menu's `Review Agent`) is where it went.
        assert!(
            work_surface::footer_actions(status_on_exit).is_empty(),
            "a finished agent's pane strip must offer no action buttons at all"
        );
    }

    /// **The regression test for the wedged centre pane.** `leave_review_tab` had exactly one
    /// non-test caller (`close_review_tab`), so every *other* way of taking over the centre pane -
    /// clicking an agent tab, opening a file, opening the graph tab - left `review_tab_active`
    /// set. `render_center_pane` checks that flag first and returns the review body
    /// unconditionally, so the tab being switched to never mounted at all, while real focus had
    /// already moved onto it: typed input went nowhere.
    ///
    /// Deliberately drives the real entry points rather than calling `leave_review_tab` - the bug
    /// was precisely that those entry points didn't call it, so a test that calls it directly
    /// (like `leaving_the_review_tab_moves_focus_off_its_handle` below) cannot catch this.
    #[gpui::test]
    fn every_way_of_leaving_the_review_tab_really_releases_the_centre_pane(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        std::fs::write(repo.path().join("agent_work.rs"), "fn main() {}\n").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        // (a) Selecting an agent tab.
        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();
        app.read_with(cx, |app, _| assert!(app.review_tab_active, "precondition"));

        app.update_in(cx, |app, window, cx| app.select_agent(id, window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(
                !app.review_tab_active,
                "clicking an agent tab must release the centre pane - otherwise the terminal \
                 never mounts and focus dangles on an unrendered handle"
            );
            assert!(!app.review_focus_handle.is_focused(window));
        });

        // (b) Opening a file tab.
        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(PathBuf::from("agent_work.rs"), window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(!app.review_tab_active, "opening a file must release it too");
            assert!(!app.review_focus_handle.is_focused(window));
        });

        // (c) Opening the git graph tab.
        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| app.open_git_graph(window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(
                !app.review_tab_active,
                "opening the graph tab must release it too - two centre-pane occupants must \
                 never both consider themselves active"
            );
            assert!(app.graph_tab_active, "and the graph tab really took over");
            assert!(!app.review_focus_handle.is_focused(window));
        });
    }

    /// **The regression test for the cancelled-capture bug.** Baseline captures used to share one
    /// `Option<Task<()>>` slot, and GPUI cancels a `Task` on drop - so spawning a second agent
    /// while the first's snapshot was still running silently destroyed the first capture, leaving
    /// that agent permanently without a review and its ref orphaned.
    ///
    /// Spawns two extra agents back to back, with no `run_until_parked` in between, so both
    /// captures are genuinely in flight simultaneously.
    ///
    /// Spawned as cheap [`ProcessKind::Shell`] processes in **separate real repositories**,
    /// rather than real `Claude`/`Codex` CLIs - spawning the actual `claude`/`codex` binaries
    /// starts heavy Node processes whose load was measurably breaking this crate's
    /// timing-sensitive inotify tests running in parallel. Immediately retagged to
    /// `ProcessKind::claude()` via `Agents::set_kind_for_test` before `capture_review_baseline`
    /// runs, since (unlike before this app drew a real type-level line between a shell and an
    /// agent session) a plain shell is never baseline-eligible at all - what this test is about,
    /// two captures genuinely in flight at once, only needs the recorded `kind`, not the real
    /// binary. Distinct worktrees, not distinct kinds, is what makes their baseline keys (and so
    /// their refs) genuinely distinct.
    #[gpui::test]
    fn concurrent_per_agent_captures_and_releases_are_never_cancelled_by_each_other(
        cx: &mut TestAppContext,
    ) {
        let repo = diverged_repo();
        let other_a = diverged_repo();
        let other_b = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let sole = sole_agent(&app, cx);

        // `Agents::spawn` + `capture_review_baseline` is exactly what `new_agent` does, minus the
        // focus/menu bookkeeping - the same pair, driven with an explicit cwd.
        let (first, second) = app.update_in(cx, |app, window, cx| {
            let first = app.agents.spawn(
                ProcessKind::Shell,
                other_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            app.agents.set_kind_for_test(first, ProcessKind::claude());
            app.capture_review_baseline(first, cx);
            // No parking in between: the first capture is still running right now.
            let second = app.agents.spawn(
                ProcessKind::Shell,
                other_b.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            app.agents.set_kind_for_test(second, ProcessKind::claude());
            app.capture_review_baseline(second, cx);
            (first, second)
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            for (label, id) in [("sole", sole), ("first", first), ("second", second)] {
                assert!(
                    app.agent_reviews.contains_key(&id),
                    "the {label} agent's baseline capture must not have been cancelled by a \
                     later agent's - a cancelled capture is never retried, so that agent would \
                     have no review for its entire lifetime"
                );
            }
            assert!(
                app._review_baseline_tasks.is_empty(),
                "and every completed capture must drop its own task slot"
            );
        });

        // Every agent's ref is real, in its own repository, and distinct from the others'.
        let refs: Vec<(PathBuf, String)> = app.read_with(cx, |app, _| {
            [sole, first, second]
                .iter()
                .map(|id| {
                    let agent = app.agents.iter().find(|a| a.id == *id).expect("agent");
                    (
                        agent.cwd.clone(),
                        app.agent_reviews[id].baseline.ref_name.clone(),
                    )
                })
                .collect()
        });
        for (cwd, ref_name) in &refs {
            assert!(
                !git_output(cwd, &["rev-parse", ref_name]).is_empty(),
                "{ref_name} must really exist in {}",
                cwd.display()
            );
        }
        let unique: std::collections::HashSet<&String> =
            refs.iter().map(|(_, name)| name).collect();
        assert_eq!(unique.len(), 3, "each agent gets its own baseline ref");

        // The mirror image, on the same two agents: closing them back to back must release
        // *both* refs. With one shared task slot the first `delete_ref` was cancelled by the
        // second and that ref leaked forever, since nothing ever retries it.
        app.update_in(cx, |app, window, cx| {
            app.close_agent(first, window, cx);
            // Again, no parking in between - the first deletion is still in flight.
            app.close_agent(second, window, cx);
        });
        cx.run_until_parked();

        for (cwd, ref_name) in refs.iter().filter(|(_, name)| {
            // The sole agent is still open, so its own ref must survive; only the two just
            // closed should be gone.
            name != &refs[0].1
        }) {
            assert!(
                git_try(cwd, &["rev-parse", "--verify", ref_name])
                    .stdout
                    .is_empty(),
                "{ref_name} must really have been deleted"
            );
        }
        assert!(
            !git_output(&refs[0].0, &["rev-parse", "--verify", &refs[0].1]).is_empty(),
            "the still-open sole agent's own baseline ref must be untouched"
        );
        app.read_with(cx, |app, _| {
            assert!(app._review_release_tasks.is_empty());
        });
    }

    /// The mirror image for ref *release*: closing two agents in quick succession used to cancel
    /// the first one's `delete_ref`, leaking that ref forever.
    /// Baseline persistence must be a real, on-disk write with real content - and must go through
    /// the background executor rather than blocking the UI thread on two `fsync`s under
    /// `persisted_state_lock`'s process-wide mutex (which background writers of the sibling state
    /// files hold across their own fsyncs).
    ///
    /// Honest about what this proves: the real, checked assertion is the on-disk *result* - the
    /// file exists, holds exactly the live baseline, and decodes back to the real worktree.
    /// "Runs off the UI thread" is asserted structurally, via the `_review_persist_task` slot
    /// only a `cx.spawn` ever fills, rather than by timing - `add_window_view` already runs the
    /// test executor, so a "not written yet" check would be measuring GPUI's scheduling rather
    /// than this code.
    #[gpui::test]
    fn baseline_persistence_really_writes_to_disk_off_the_ui_thread(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let config = tempfile::tempdir().expect("config dir");
        let settings_path = config.path().join("settings.toml");
        let baselines_path =
            crate::review::baseline_state::review_baseline_path_for(&settings_path);

        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo.path().to_path_buf()),
                true,
                crate::settings::store::Settings::default(),
                Some(settings_path.clone()),
                window,
                cx,
            )
        });

        cx.run_until_parked();
        // The window's own startup shell (see `sole_agent`'s docs) is never baseline-eligible,
        // so it never persists anything - this replaces it with a real agent before checking
        // that a baseline was written at all.
        let id = sole_agent(&app, cx);

        assert!(
            baselines_path.exists(),
            "the baseline must really be persisted to disk"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app._review_persist_task.is_some(),
                "and the save must have gone through a real `cx.spawn` task - an inline \
                 `save_merged_at` would leave this slot empty while fsync'ing on the UI thread"
            );
        });
        let state = crate::review::baseline_state::ReviewBaselineState::load_at(&baselines_path);
        assert_eq!(
            state.baselines.len(),
            1,
            "exactly the sole agent's baseline"
        );
        let entry = state.baselines.values().next().expect("an entry");
        assert_eq!(
            entry.worktree_path(),
            Some(repo.path().to_path_buf()),
            "recorded against the real worktree, decodably"
        );
        app.read_with(cx, |app, _| {
            let agent = app
                .agents
                .iter()
                .find(|a| a.id == id)
                .expect("the sole agent");
            let ProcessKind::Agent(kind) = agent.kind else {
                unreachable!("sole_agent always spawns a real agent, never a shell");
            };
            let key = crate::review::state::baseline_key(&agent.cwd, kind, agent.spawned_at_unix);
            assert_eq!(
                state.get(&key),
                Some(app.agent_reviews[&agent.id].baseline.clone()),
                "and the persisted baseline must match the live one exactly"
            );
        });
    }

    /// Leaving the review tab must move real keyboard focus off a handle that is about to stop
    /// being rendered - the dangling-focus bug class `leave_graph_tab`'s own docs describe, which
    /// this surface copies its discipline from.
    #[gpui::test]
    fn leaving_the_review_tab_moves_focus_off_its_handle(cx: &mut TestAppContext) {
        let repo = diverged_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let id = sole_agent(&app, cx);

        app.update_in(cx, |app, window, cx| app.open_review_tab(id, window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(
                app.review_focus_handle.is_focused(window),
                "opening the review tab must really move focus onto its own handle"
            );
        });

        app.update_in(cx, |app, window, cx| app.leave_review_tab(window, cx));
        cx.run_until_parked();
        app.update_in(cx, |app, window, _cx| {
            assert!(!app.review_tab_active);
            assert!(
                !app.review_focus_handle.is_focused(window),
                "the handle stops being track_focus'd the moment the tab stops rendering, so \
                 focus must already have moved off it"
            );
        });
    }
}
