//! The real run-transcript centre tab (GitHub issue #227): its strip entry, its header, its
//! dimmed body, and its footer's `Resume here` / `Start a new agent from this`.

use super::*;

use crate::root::widgets::text_tooltip;
use crate::run_history::model;
use crate::work_surface::render::{DraggedTab, TabChromeArgs};
use crate::work_surface::state as work_surface;

impl AdeApp {
    /// Opens (or replaces) the run-transcript tab in `worktree`, showing the run `run_key`.
    pub(crate) fn open_run_tab(
        &mut self,
        worktree: PathBuf,
        run_key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A run whose record has gone (pruned out of `agent-status.toml` between the render and
        // the click) has nothing to show. Refusing is honest; opening an empty pane is not.
        if crate::hooks::history::find(&self.agent_status_state, &run_key).is_none() {
            return;
        }
        if self.palette_open {
            self.close_palette(window, cx);
        }
        // Before anything below sets `run_tab_active`: `select_worktree` itself leaves the run tab
        // (a run tab belongs to the worktree being left), so doing this after would immediately
        // undo it.
        if self.current_worktree_path().as_deref() != Some(worktree.as_path()) {
            self.select_worktree_by_path(&worktree, window, cx);
        }
        // Every other centre surface must be *left*, not merely stopped being drawn - see this
        // module's docs, and `crate::review::render::AdeApp::leave_review_tab`'s own.
        self.leave_graph_tab(window, cx);
        self.graph_tab_open = false;
        self.leave_review_tab(window, cx);
        self.review_tab_open = None;

        let was_active =
            self.run_tab_active && self.run_tab_by_worktree.get(&worktree) == Some(&run_key);
        self.run_tab_by_worktree.insert(worktree, run_key.clone());
        self.run_tab_active = true;
        self.open_change = None;
        self.plus_menu_open = false;
        self.title_menu_open = None;
        self.prune_confirm_armed = false;

        // The same "leaving Files"/"leaving the code surface" sweep `open_review_tab` performs,
        // for the identical reason: `self.open_change = None` above just unrendered the code
        // surface this tab is replacing.
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
        self.refresh_open_diff_file_cache();

        if !was_active && !self.focus_is_on_an_overlay(window, cx) {
            self.run_tab_focus.capture(window, &self.agents, cx);
        }
        window.focus(&self.run_tab_focus_handle, cx);

        // The one real read this surface needs, asked for once per run - see
        // `crate::run_history::flow::AdeApp::load_run_transcript`.
        self.load_run_transcript(run_key, cx);
        // A freshly opened transcript starts at its own beginning, not at wherever the last one
        // was left: the handle is shared across runs (there is only ever one of these tabs), so
        // without this a short run would open scrolled past its own end.
        self.run_tab_scroll_handle
            .set_offset(gpui::Point::default());
        cx.notify();
    }

    /// Closes the run tab outright (its `×`), removing it from this worktree's strip. The stored
    /// transcript is untouched - closing a recording does not delete it.
    pub(crate) fn close_run_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.leave_run_tab(window, cx);
        if let Some(worktree) = self.current_worktree_path() {
            self.run_tab_by_worktree.remove(&worktree);
        }
        cx.notify();
    }

    /// Common bookkeeping whenever the run tab stops being the active centre-pane content -
    /// selecting an agent/file/graph/review tab while it was showing, or closing it outright. A
    /// no-op if it wasn't active.
    pub(crate) fn leave_run_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.run_tab_active {
            return;
        }
        self.run_tab_active = false;
        if self.run_tab_focus_handle.is_focused(window) {
            let fallback = self.focus_fallback_handle();
            restore_focus(&self.agents, &mut self.run_tab_focus, fallback, window, cx);
        }
        self.palette_focus.forget_target(&self.run_tab_focus_handle);
        self.settings_focus
            .forget_target(&self.run_tab_focus_handle);
        self.code_focus.forget_target(&self.run_tab_focus_handle);
        self.graph_focus.forget_target(&self.run_tab_focus_handle);
        self.review_focus.forget_target(&self.run_tab_focus_handle);
    }

    /// Which run this worktree's run tab is showing, if it has one open.
    pub(crate) fn open_run_key(&self) -> Option<String> {
        let worktree = self.current_worktree_path()?;
        self.run_tab_by_worktree.get(&worktree).cloned()
    }

    /// The record behind [`Self::open_run_key`], re-read from the store on every render rather
    /// than snapshotted at open time: a run that finished while its own tab was open (the
    /// `Archive run` path) gains its real ending and diffstat, and the header must show them
    /// rather than the state it was opened in.
    fn open_run(&self) -> Option<crate::hooks::history::PastAgent> {
        crate::hooks::history::find(&self.agent_status_state, &self.open_run_key()?)
    }

    /// The whole run-transcript body: header, dimmed transcript, footer.
    pub(crate) fn render_run_view(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(run) = self.open_run() else {
            // The record went away underneath an open tab (pruned by
            // `AgentStatusState::prune_to_most_recent`). Say so rather than paint a blank pane.
            return div()
                .id("run-view")
                .track_focus(&self.run_tab_focus_handle)
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .bg(theme::surface::CENTER)
                .child(crate::root::widgets::render_sidebar_message(
                    "This run is no longer in the history.".to_string(),
                    theme::text::GHOST.into(),
                ))
                .into_any_element();
        };

        let now_unix = crate::run_history::unix_now();
        let branch = self
            .repos
            .iter()
            .flat_map(|repo| repo.worktrees.iter())
            .find(|item| item.path == run.worktree)
            .and_then(|item| item.branch.clone());
        let captured = self
            .run_transcripts
            .get(&run.key)
            .cloned()
            .flatten()
            .unwrap_or_default();
        let lines = model::transcript_body(
            &run,
            branch.as_deref(),
            (!captured.is_empty()).then_some(captured.as_slice()),
            now_unix,
        );
        let drift = self
            .run_drift
            .get(&run.worktree)
            .and_then(|counts| counts.get(&run.key).copied());

        div()
            .id("run-view")
            .debug_selector(|| "run-view".to_string())
            .track_focus(&self.run_tab_focus_handle)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(theme::surface::CENTER)
            .child(self.render_run_header(&run, now_unix))
            .child(self.render_run_transcript(&lines, cx))
            .child(self.render_run_footer(&run, drift, cx))
            .into_any_element()
    }

    /// §3's header: "agent chip · title · `<agent> · <when> · 24m · 21 turns · 6 files · +148
    /// −96` · outcome pill".
    fn render_run_header(
        &self,
        run: &crate::hooks::history::PastAgent,
        now_unix: i64,
    ) -> impl IntoElement + use<> {
        let outcome = model::Outcome::of(run);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .h(theme::band::CONTEXT_BAR)
            // The same band and the same fill the agent context bar one surface over uses - a
            // centre pane's header row is one control, drawn wherever the centre pane is.
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .debug_selector(|| "run-header".to_string())
            .child(self.render_agent_chip_icon(
                ProcessKind::Agent(run.kind),
                px(15.0),
                self.ui_text_size(9.0),
            ))
            .child(
                div()
                    .flex_none()
                    .max_w(px(320.0))
                    .truncate()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(11.5))
                    .text_color(theme::text::SELECTED)
                    .child(model::run_title(run)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::FAINT)
                    .debug_selector(|| "run-header-meta".to_string())
                    .child(model::run_meta_line(run, now_unix)),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .bg(outcome.bg())
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(outcome.fg())
                    .debug_selector(|| "run-outcome-pill".to_string())
                    .child(outcome.label()),
            )
    }

    /// The transcript itself, at §3's 70% opacity - "the one signal that this is a recording, not
    /// a live pane".
    fn render_run_transcript(
        &self,
        lines: &[model::TranscriptLine],
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        div()
            .relative()
            .flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .child(
                div()
                    .id("run-transcript")
                    .debug_selector(|| "run-transcript".to_string())
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.run_tab_scroll_handle)
                    .px(px(14.0))
                    .py(px(10.0))
                    .opacity(theme::history::TRANSCRIPT_OPACITY)
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(11.0))
                    .children(lines.iter().map(|line| {
                        div()
                            .text_color(line.tone.color())
                            // An empty line still has to occupy one: a `div` with no child has no
                            // intrinsic height, so the blank the model puts before the closing
                            // line would silently vanish and the two would run together.
                            .child(if line.text.is_empty() {
                                gpui::SharedString::from("\u{a0}")
                            } else {
                                gpui::SharedString::from(line.text.clone())
                            })
                    })),
            )
            .children(crate::root::scrollbar::render_vertical_scrollbar(
                "run-transcript-scrollbar",
                &self.run_tab_scroll_handle,
                &[],
                cx,
            ))
    }

    /// §3's footer: "drift dot, the consequence sentence, **Resume here** ... and `Start a new
    /// agent from this`."
    fn render_run_footer(
        &self,
        run: &crate::hooks::history::PastAgent,
        drift: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let resume_key = run.key.clone();
        let kind = run.kind;
        let worktree = run.worktree.clone();

        let mut bar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .px(px(12.0))
            .h(theme::band::SURFACE_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER)
            .debug_selector(|| "run-footer".to_string());

        if let Some(commits) = drift {
            let band = model::DriftBand::of(commits);
            bar = bar
                .child(
                    div()
                        .flex_none()
                        .w(px(5.0))
                        .h(px(5.0))
                        .rounded_full()
                        .bg(band.dot())
                        .debug_selector(|| "run-drift-dot".to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font(font(theme::font::SANS))
                        .text_size(self.ui_text_size(10.5))
                        .text_color(band.text())
                        .debug_selector(|| "run-drift-sentence".to_string())
                        .child(model::drift_sentence(commits)),
                );
        } else {
            bar = bar.child(div().flex_1().min_w_0());
        }

        bar.child(
            div()
                .id("run-resume")
                .debug_selector(|| "run-resume".to_string())
                .flex_none()
                .cursor_pointer()
                .px(px(9.0))
                .py(px(3.0))
                .rounded(theme::radius::BUTTON)
                .border_1()
                .border_color(theme::history::RESUME_BORDER)
                .bg(theme::history::RESUME_BG)
                .hover(|el| el.bg(theme::history::RESUME_BG_HOVER))
                .font(font(theme::font::SANS))
                .text_size(self.ui_text_size(10.5))
                .text_color(theme::history::RESUME_FG)
                .tooltip(text_tooltip(
                    "Continues this run's own conversation in its checkout, where one was recorded",
                ))
                .child("Resume here")
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    if this.resume_past_agent(&resume_key, window, cx) {
                        // The resumed agent is now the centre pane's real content; leaving this
                        // tab is what `resume_past_agent`'s own `focus_newly_spawned_agent` needs
                        // to have any effect (see this module's docs on the focus discipline).
                        this.leave_run_tab(window, cx);
                    }
                })),
        )
        .child(
            div()
                .id("run-start-new")
                .debug_selector(|| "run-start-new".to_string())
                .flex_none()
                .cursor_pointer()
                .px(px(9.0))
                .py(px(3.0))
                .rounded(theme::radius::BUTTON)
                // The app's own outline action button (`work_surface::action_button_colors`'s
                // `Outline`): no fill, `border::BUTTON`, `text::SECONDARY`. Secondary beside the
                // green primary, which is what the pair is.
                .border_1()
                .border_color(theme::border::BUTTON)
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .font(font(theme::font::SANS))
                .text_size(self.ui_text_size(10.5))
                .text_color(theme::text::SECONDARY)
                .tooltip(text_tooltip(
                    "A fresh agent of the same kind in this checkout - it carries none of this \
                     run's conversation",
                ))
                .child("Start a new agent from this")
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.select_worktree_by_path(&worktree, window, cx);
                    this.leave_run_tab(window, cx);
                    this.new_agent(ProcessKind::Agent(kind), window, cx);
                })),
        )
    }
}

/// The tab strip's own run entry, rendered only for [`work_surface::TabRef::Run`] - which
/// `work_surface::state::reconcile_tab_order` only ever produces while this worktree really has
/// one open.
pub(crate) fn render_run_tab(app: &AdeApp, cx: &mut Context<AdeApp>) -> gpui::AnyElement {
    let is_active = app.run_tab_active;
    let colors = crate::work_surface::state::tab_colors(is_active);
    let close_color = if is_active {
        theme::text::DIMMER
    } else {
        theme::text::DISABLED
    };
    let run = app
        .open_run_key()
        .and_then(|key| crate::hooks::history::find(&app.agent_status_state, &key));
    let (label, chip) = match &run {
        Some(run) => (
            model::run_tab_label(run, crate::run_history::unix_now()),
            app.render_agent_chip_icon(
                ProcessKind::Agent(run.kind),
                px(14.0),
                app.ui_text_size(8.5),
            ),
        ),
        // Unreachable in practice - `reconcile_tab_order` only yields `Run` when the map holds a
        // key - but a record can be pruned between two frames, and a tab that renamed itself to
        // nothing would be worse than one that says what it is.
        None => ("run".to_string(), gpui::Empty.into_any_element()),
    };

    let close_button = app.render_tab_close_button(
        "close-run-tab",
        close_color,
        None,
        |this, window, cx| {
            this.close_run_tab(window, cx);
        },
        cx,
    );

    let (label_element, label_tooltip) = app.render_tab_label(
        label.clone(),
        theme::font::MONO,
        app.ui_text_size(11.0),
        colors.label,
        cx,
    );
    let content: Vec<gpui::AnyElement> = vec![chip, label_element, close_button.into_any_element()];

    app.render_tab_chrome(
        TabChromeArgs {
            outer_id: "run-tab".into(),
            hit_id: "run-tab-hit".into(),
            tab_ref: work_surface::TabRef::Run,
            drag_value: DraggedTab::Run { label },
            is_active,
            content,
            label_tooltip,
            on_middle_click: Box::new(|this, window, cx| {
                this.close_run_tab(window, cx);
            }),
            on_activate: Box::new(move |this, window, cx| {
                let Some(worktree) = this.current_worktree_path() else {
                    return;
                };
                let Some(key) = this.run_tab_by_worktree.get(&worktree).cloned() else {
                    return;
                };
                this.open_run_tab(worktree, key, window, cx);
            }),
            debug_selector: Some("run-tab"),
        },
        cx,
    )
}
