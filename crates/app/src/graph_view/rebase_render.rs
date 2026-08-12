//! Interactive rebase - GitHub issue #242 phase B: the real GPUI surface. `crate::graph_view::
//! rebase` owns the mode's state and every mutation; this file only ever reads it. Swaps in for
//! the graph pane's ordinary commit list/right panel (design spec §1) - see
//! `crate::graph_view::render::AdeApp::render_graph_view`/`render_graph_right_panel`'s own short
//! -circuits into [`AdeApp::render_rebase_view`]/[`AdeApp::render_rebase_result_panel`].

use super::rebase::{
    derive_result_blocks, derive_result_commit_count, derive_stop_count, outcome_stopped_commit,
    RebaseActionKind, RebaseModeState, RebasePhase, ResultBlock, ResultBlockStatus,
};
use super::*;
use gpui::{DragMoveEvent, KeyDownEvent};
use wt_core::rebase::RebaseOutcome;

/// The dragged payload for a plan row's own drag handle (design spec §1.4) - mirrors
/// `work_surface::render::DraggedTab`'s shape (a real commit id, `Clone`+`Render` for GPUI's own
/// drag-ghost machinery).
#[derive(Clone)]
pub(crate) struct DraggedRebaseRow(pub String);

impl gpui::Render for DraggedRebaseRow {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.0))
            .py(px(4.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::PALETTE)
            .border_1()
            .border_color(theme::border::POPOVER)
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::HEADING)
            .child(self.0.chars().take(7).collect::<String>())
    }
}

impl AdeApp {
    /// The graph pane's whole interactive-rebase surface (design spec §1): banner, the Stopped-
    /// phase strip (only while stopped on a real conflict), the plan's own column header, and the
    /// plan row list. Called from `Self::render_graph_view` instead of the ordinary toolbar/
    /// commit-list body whenever `self.graph_state.rebase` is `Some`.
    pub(crate) fn render_rebase_view(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut container = div()
            .id("graph-rebase-view")
            .debug_selector(|| "graph-rebase-view".to_string())
            .track_focus(&self.graph_focus_handle)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .bg(theme::surface::CENTER)
            .child(self.render_rebase_banner(cx));

        if let Some(strip) = self.render_rebase_stopped_strip(cx) {
            container = container.child(strip);
        }

        container
            .child(render_rebase_column_header())
            .child(self.render_rebase_plan_rows(cx))
            .into_any_element()
    }

    /// Design spec §1.2: `rb` chip, title, subtitle, and the phase-specific action row.
    fn render_rebase_banner(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        // "onto <target>" names the real commit this plan bases onto - the graph row's own short
        // sha is the only real, always-available label for it (no separate branch/tag name is
        // guaranteed to point at it).
        let subtitle = format!(
            "{} \u{b7} onto {}",
            rebase_state.branch, rebase_state.onto_short
        );

        div()
            .id("rebase-banner")
            .debug_selector(|| "rebase-banner".to_string())
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::graph::REBASE_BANNER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::graph::TAB_CHIP_BG)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(9.5))
                    .text_color(theme::graph::TAB_CHIP_FG)
                    .child("rb"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(12.0))
                            .text_color(theme::text::HEADING)
                            .child("Interactive rebase"),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .truncate()
                            .child(subtitle),
                    ),
            )
            .child(div().flex_1())
            .child(self.render_rebase_banner_actions(rebase_state, cx))
            .into_any_element()
    }

    fn render_rebase_banner_actions(
        &self,
        rebase_state: &RebaseModeState,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // GitHub issue #242 phase B fix: an independent review reproduced two real bugs traced
        // back to buttons staying clickable while a real background operation (the initial plan
        // load, or a real `wt_core::rebase` call) was already in flight - a double-click on
        // Continue could amend the wrong commit (task B's stale message landing on the *new*
        // HEAD task A's own amend had already advanced past), and Cancel had no guard at all, so
        // clicking it while `Start rebase`'s subprocess was still running abandoned a real,
        // running rebase with no banner left to recover it from. Disabling every banner button -
        // Cancel included - for the whole duration of `op_in_flight` closes both: `Self::
        // cancel_rebase_mode`/`Self::continue_rebase`/`Self::skip_rebase` all guard on the same
        // flag now too, so a disabled button and a guarded handler can never disagree.
        let enabled = !rebase_state.op_in_flight;
        match &rebase_state.phase {
            RebasePhase::Planning => {
                let (n, m) = (
                    rebase_state.plan.len(),
                    derive_result_commit_count(&rebase_state.plan),
                );
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.5))
                            .text_color(theme::text::DIM)
                            .child(format!("{n} \u{2192} {m} commits")),
                    )
                    .child(
                        render_rebase_button("Cancel", false, enabled).when(enabled, |el| {
                            el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.cancel_rebase_mode(cx);
                            }))
                        }),
                    )
                    .child(render_rebase_button("Start rebase", true, enabled).when(
                        enabled,
                        |el| {
                            el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.start_rebase(cx);
                            }))
                        },
                    ))
                    .into_any_element()
            }
            RebasePhase::Stopped { .. } => div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    render_rebase_button("Abort", false, enabled).when(enabled, |el| {
                        el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.abort_rebase(cx);
                        }))
                    }),
                )
                .child(
                    render_rebase_button("Skip", false, enabled).when(enabled, |el| {
                        el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.skip_rebase(cx);
                        }))
                    }),
                )
                .child(
                    render_rebase_button("Continue", true, enabled).when(enabled, |el| {
                        el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.continue_rebase(cx);
                        }))
                    }),
                )
                .into_any_element(),
        }
    }

    /// Design spec §1.7: the Stopped-phase strip under the main banner, only while the real
    /// `RebaseOutcome::StoppedForConflict` gives a genuine conflict to report -
    /// `StoppedForEdit`/planning renders no strip at all (the row-level pause markers already
    /// communicate where a non-conflict stop landed - see that section's own docs).
    fn render_rebase_stopped_strip(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let rebase_state = self.graph_state.rebase.as_ref()?;
        let RebasePhase::Stopped {
            outcome:
                RebaseOutcome::StoppedForConflict {
                    commit,
                    conflicted_files,
                },
        } = &rebase_state.phase
        else {
            return None;
        };
        let total = rebase_state.plan.len();
        let stopped_at = rebase_state
            .plan
            .iter()
            .position(|row| &row.commit == commit)
            .map(|index| index + 1)
            .unwrap_or(0);
        let file_label = conflicted_files
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_default();

        Some(
            div()
                .id("rebase-stopped-strip")
                .debug_selector(|| "rebase-stopped-strip".to_string())
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(6.0))
                .bg(theme::status::BANNER_BG)
                .border_b_1()
                .border_color(theme::status::BANNER_BORDER)
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::status::FAIL)
                        .child(format!(
                            "stopped at {stopped_at} of {total} \u{b7} {} conflict(s) in {file_label}",
                            conflicted_files.len()
                        )),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("rebase-resolve-conflict-link")
                        .debug_selector(|| "rebase-resolve-conflict-link".to_string())
                        .cursor_pointer()
                        .font(font(theme::font::SANS))
                        .text_size(px(10.5))
                        .text_color(theme::button::BLUE_FG)
                        .child("Resolve in the diff view")
                        .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                            this.resolve_rebase_conflict_in_diff_view(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }

    /// Design spec §1.4: the scrollable plan row list, oldest first, top to bottom.
    fn render_rebase_plan_rows(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        if rebase_state.op_in_flight && rebase_state.plan.is_empty() {
            return div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .font(font(theme::font::SANS))
                .text_size(px(11.5))
                .text_color(theme::text::FAINT)
                .child("loading plan\u{2026}")
                .into_any_element();
        }
        let count = rebase_state.plan.len();
        let rows: Vec<gpui::AnyElement> = (0..count)
            .map(|index| self.render_rebase_plan_row(index, cx))
            .collect();

        div()
            .id("rebase-plan-rows")
            .debug_selector(|| "rebase-plan-rows".to_string())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // Every row's own action chip and its dropdown options already `cx.stop_propagation()`
            // (see `Self::render_rebase_action_chip`/`Self::render_rebase_action_menu`), so a
            // click that reaches this far was never on either of those - closing whatever action
            // menu happens to be open here is exactly the same "click elsewhere dismisses the
            // popover" contract `Self::render_graph_row_menu`'s own scrim gives the row `⋯` menu.
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.close_rebase_action_menu(cx);
            }))
            .children(rows)
            .child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .py(px(8.0))
                    .font(font(theme::font::MONO))
                    .text_size(px(9.5))
                    .text_color(theme::text::GHOST)
                    .child("oldest first \u{b7} applied top to bottom"),
            )
            .into_any_element()
    }

    fn render_rebase_plan_row(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let Some(row) = rebase_state.plan.get(index) else {
            return gpui::Empty.into_any_element();
        };
        let commit = row.commit.clone();
        let is_folded = row.action.folds_into_previous();
        let is_dropped = row.action == RebaseActionKind::Drop;
        let is_reword = row.action == RebaseActionKind::Reword;
        let planned_pause = row.is_planned_pause();
        let actual_stop = match &rebase_state.phase {
            RebasePhase::Stopped { outcome } => {
                outcome_stopped_commit(outcome) == Some(commit.as_str())
            }
            RebasePhase::Planning => false,
        };
        let files_label = row
            .files_changed
            .map(|n| n.to_string())
            .unwrap_or_else(|| "\u{2014}".to_string());
        let action_menu_open = rebase_state.action_menu_open == Some(index);
        let dragging_this = rebase_state.dragging_row.as_deref() == Some(commit.as_str());
        let insertion_caret = rebase_state
            .drag_insertion
            .as_ref()
            .filter(|(hovered, _)| hovered == &commit)
            .map(|(_, after)| *after);

        let drag_value = DraggedRebaseRow(commit.clone());
        let commit_for_drag = commit.clone();
        let commit_for_drag_move = commit.clone();
        let commit_for_drop = commit.clone();
        // `on_drag`'s own constructor closure only ever receives `&mut App` (not a `cx.listener`
        // reaching `Context<Self>`), so recording the drag into app state needs a real entity
        // handle captured up front and driven through `Entity::update` - the exact pattern
        // `work_surface::render::AdeApp::render_tab_chrome`'s own `on_drag` uses for
        // `start_dragging_tab`.
        let this_entity = cx.entity();

        let row_el = div()
            .id(("rebase-plan-row", index))
            .debug_selector(move || format!("rebase-plan-row-{index}"))
            .relative()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(theme::graph::ROW)
            .px(px(10.0))
            .border_b_1()
            .border_color(theme::border::INNER)
            .when(dragging_this, |el| el.opacity(0.4))
            .on_drag(drag_value, move |dragged, _position, _window, cx| {
                this_entity.update(cx, |this, cx| {
                    this.start_dragging_rebase_row(commit_for_drag.clone(), cx);
                });
                cx.new(|_| dragged.clone())
            })
            .on_drag_move(cx.listener(
                move |this, event: &DragMoveEvent<DraggedRebaseRow>, _window, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let insert_after = event.event.position.x >= event.bounds.center().x;
                    this.update_rebase_row_drag_insertion(&commit_for_drag_move, insert_after, cx);
                },
            ))
            .on_drop(
                cx.listener(move |this, dragged: &DraggedRebaseRow, _window, cx| {
                    this.drop_dragged_rebase_row(dragged.0.clone(), commit_for_drop.clone(), cx);
                }),
            );

        row_el
            .when_some(insertion_caret, |el, after| {
                el.child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .bottom(px(0.0))
                        .when(after, |el| el.right(px(0.0)))
                        .when(!after, |el| el.left(px(0.0)))
                        .w(px(2.0))
                        .bg(theme::button::BLUE_FG),
                )
            })
            .child(
                div()
                    .flex_none()
                    .w(px(14.0))
                    .cursor_grab()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::GHOST)
                    .child("\u{2237}"),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(14.0))
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::GHOST)
                    .child(if is_folded { "\u{2514}" } else { "" }),
            )
            .child(self.render_rebase_action_chip(index, row.action, cx))
            .child(if is_reword {
                self.render_rebase_reword_field(index, row, cx)
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .when(is_folded, |el| el.text_color(theme::text::FAINTER))
                    .when(is_dropped, |el| {
                        el.text_color(theme::text::FAINTER).line_through()
                    })
                    .when(!is_folded && !is_dropped, |el| {
                        el.text_color(theme::text::BODY)
                    })
                    .child(row.original_subject.clone())
                    .into_any_element()
            })
            .child(
                div()
                    .flex_none()
                    .w(px(36.0))
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(files_label),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(56.0))
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::FAINTER)
                    .child(row.short_sha.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(18.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(render_rebase_pause_marker(planned_pause, actual_stop)),
            )
            .when(action_menu_open, |el| {
                el.child(self.render_rebase_action_menu(index, cx))
            })
            .into_any_element()
    }

    fn render_rebase_action_chip(
        &self,
        index: usize,
        action: RebaseActionKind,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(("rebase-action-chip", index))
            .debug_selector(move || format!("rebase-action-chip-{index}"))
            .flex_none()
            .w(px(76.0))
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::CHIP_NEUTRAL)
            .cursor_pointer()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(3.0))
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::HEADING)
            .child(action.label())
            .child(div().text_color(theme::text::GHOST).child("\u{25be}"))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.toggle_rebase_action_menu(index, cx);
            }))
            .into_any_element()
    }

    /// The action chip's own dropdown - real `pick`/`reword`/`edit`/`squash`/`fixup`/`drop`
    /// options, positioned via a plain `.relative()`/`.absolute()` nesting inside the row itself
    /// (unlike the row `⋯`/Push menus, this never needs window-space bounds: it never has to
    /// escape its own row's layout box - see `crate::graph_view::render::render_graph_row_menu`'s
    /// own docs for why *that* popover needs the heavier window-space mechanism instead).
    fn render_rebase_action_menu(&self, index: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mut menu = div()
            .id(("rebase-action-menu", index))
            .absolute()
            .top(theme::graph::ROW)
            .left(px(10.0))
            .w(px(90.0))
            .py(px(3.0))
            .occlude()
            .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
            }));
        menu = crate::root::widgets::menu_popover_chrome(menu, theme::shadow::MENU);
        for kind in RebaseActionKind::ALL {
            menu = menu.child(
                div()
                    .id(format!("rebase-action-option-{index}-{}", kind.label()))
                    .debug_selector(move || {
                        format!("rebase-action-option-{index}-{}", kind.label())
                    })
                    .px(px(8.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::HEADING)
                    .child(kind.label())
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.set_rebase_row_action(index, kind, cx);
                    })),
            );
        }
        menu.into_any_element()
    }

    /// Design spec §1.4: a reword row's subject replaced by a real single-line text input,
    /// pre-filled with the current subject - the same hand-rolled `text_history::TextField` +
    /// manual caret idiom `crate::root::new_file`'s prompt established (see that module's own
    /// docs for why: no real `EntityInputHandler` exists in this app for a single-line field).
    fn render_rebase_reword_field(
        &self,
        index: usize,
        row: &super::rebase::RebasePlanRow,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let text = row.reword_message.as_str().to_string();
        let focus_handle = row.reword_focus_handle.clone();
        div()
            .id(("rebase-reword-field", index))
            .flex_1()
            .min_w_0()
            .track_focus(&focus_handle)
            .key_context("text-input")
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.handle_rebase_reword_key_down(index, event, window, cx);
            }))
            .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
            }))
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::SEGMENT_TRACK)
            .flex()
            .items_center()
            .gap(px(1.0))
            .font(font(theme::font::MONO))
            .text_size(px(11.0))
            .text_color(theme::text::BODY)
            .child(text)
            .child(self.render_simple_input_caret("rebase-reword-caret", &focus_handle))
            .into_any_element()
    }

    /// The right sidebar's Result panel (design spec §1.6) - replaces the ordinary Commit/
    /// Branches panel entirely while in rebase mode. `crate::graph_view::render::
    /// render_graph_right_panel` short-circuits into this before building the Commit/Branches
    /// toggle at all.
    pub(crate) fn render_rebase_result_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(rebase_state) = self.graph_state.rebase.as_ref() else {
            return gpui::Empty.into_any_element();
        };
        let blocks = derive_result_blocks(&rebase_state.plan);
        let count = blocks.len();

        let mut panel = div()
            .id("rebase-result-panel")
            .debug_selector(|| "rebase-result-panel".to_string())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .child(
                div()
                    .flex_none()
                    .px(px(10.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(10.5))
                    .text_color(theme::text::HEADING)
                    .child("RESULT")
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::DIM)
                            .child(format!(
                                "{} \u{2192} {count} commits",
                                rebase_state.plan.len()
                            )),
                    ),
            );

        for block in &blocks {
            panel = panel.child(render_rebase_result_block(block));
        }

        panel = panel.child(div().flex_1());

        if let Some(warning) = self.render_rebase_agent_warning(rebase_state.op_in_flight, cx) {
            panel = panel.child(warning);
        }
        if let Some(warning) = render_rebase_remote_warning(rebase_state) {
            panel = panel.child(warning);
        }
        if let Some(warning) = render_rebase_stop_count_warning(rebase_state) {
            panel = panel.child(warning);
        }

        panel.into_any_element()
    }

    /// Design spec §1.6 warning 1 - only rendered when this pane's own worktree really has at
    /// least one running agent (`crate::work_surface::agents::Agents::count_for_cwd`-style live
    /// check, `is_agent_session` filtered - see `Self::pause_rebase_agents`'s own docs for why a
    /// bare shell never counts). `op_in_flight` disables the real `Pause now` click (no
    /// `.on_click` attached at all while `true`) for the same double-click-safety reason every
    /// other banner button is disabled during any in-flight operation - see `Self::
    /// render_rebase_banner_actions`'s own docs.
    fn render_rebase_agent_warning(
        &self,
        op_in_flight: bool,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let cwd = self.diff_root.clone();
        let running = self
            .agents
            .iter_for_cwd(cwd)
            .filter(|agent| agent.kind.is_agent_session())
            .count();
        if running == 0 {
            return None;
        }
        let enabled = !op_in_flight;
        Some(
            render_rebase_warning_shell(format!(
                "{running} agent(s) running in this worktree - a rebase rewrites files under \
                 them."
            ))
            .child(
                div()
                    .id("rebase-pause-agents")
                    .debug_selector(|| "rebase-pause-agents".to_string())
                    .px(px(8.0))
                    .py(px(3.0))
                    .rounded(theme::radius::CHIP)
                    .when(enabled, |el| el.cursor_pointer())
                    .when(!enabled, |el| el.cursor_default().opacity(0.5))
                    .bg(theme::status::ASK_BG)
                    .font(font(theme::font::SANS))
                    .text_size(px(10.0))
                    .text_color(theme::status::ASK)
                    .child("Pause now")
                    .when(enabled, |el| {
                        el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                            this.pause_rebase_agents(cx);
                        }))
                    }),
            )
            .into_any_element(),
        )
    }
}

fn render_rebase_result_block(block: &ResultBlock) -> gpui::AnyElement {
    let status_label = match block.status {
        ResultBlockStatus::Normal => None,
        ResultBlockStatus::Reworded => Some("reworded"),
        ResultBlockStatus::StopsForMessage => Some("stops for a message"),
        ResultBlockStatus::StopsToAmend => Some("stops to amend"),
    };
    div()
        .flex_none()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .px(px(10.0))
        .py(px(6.0))
        .border_b_1()
        .border_color(theme::border::INNER)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font(font(theme::font::SANS))
                        .text_size(px(11.0))
                        .text_color(theme::text::BODY)
                        .child(block.subject.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .text_size(px(9.5))
                        .text_color(theme::text::FAINTER)
                        .child(block.short_sha.clone()),
                ),
        )
        .when(block.folded_count > 0 || status_label.is_some(), |el| {
            el.child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(9.5))
                    .text_color(theme::text::GHOST)
                    .child(match (block.folded_count, status_label) {
                        (0, Some(status)) => status.to_string(),
                        (n, Some(status)) => format!("{n} commits folded in \u{b7} {status}"),
                        (n, None) => format!("{n} commits folded in"),
                    }),
            )
        })
        .into_any_element()
}

fn render_rebase_remote_warning(rebase_state: &RebaseModeState) -> Option<gpui::AnyElement> {
    let count = rebase_state.already_on_upstream.len();
    if count == 0 {
        return None;
    }
    Some(
        render_rebase_warning_shell(format!(
            "{count} commit(s) in this plan are already on the tracked remote branch - a \
             force-with-lease push will be needed afterward."
        ))
        .into_any_element(),
    )
}

fn render_rebase_stop_count_warning(rebase_state: &RebaseModeState) -> Option<gpui::AnyElement> {
    let n = derive_stop_count(&rebase_state.plan);
    if n == 0 {
        return None;
    }
    Some(
        render_rebase_warning_shell(format!(
            "Stops {n} time(s) - each `edit` row and each message-less `reword` row hands \
             control back to you before the rebase continues."
        ))
        .into_any_element(),
    )
}

fn render_rebase_warning_shell(text: String) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(10.0))
        .py(px(6.0))
        .border_t_1()
        .border_color(theme::status::BANNER_BORDER)
        .bg(theme::status::BANNER_BG)
        .child(
            div()
                .flex_1()
                .font(font(theme::font::SANS))
                .text_size(px(10.0))
                .text_color(theme::status::ASK)
                .child(text),
        )
}

/// `enabled` reflects `!op_in_flight` at every real call site (see `Self::
/// render_rebase_banner_actions`'s own docs) - a disabled button renders dimmed, with no
/// `cursor_pointer`, and (the caller's own job - see that method) no `.on_click` attached at all,
/// so it is genuinely inert, not just styled to look that way.
fn render_rebase_button(
    label: &'static str,
    primary: bool,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(format!("rebase-banner-button-{label}"))
        .debug_selector(move || format!("rebase-banner-button-{label}"))
        .px(px(10.0))
        .h(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius::BUTTON)
        .when(enabled, |el| el.cursor_pointer())
        .when(!enabled, |el| el.cursor_default().opacity(0.45))
        .when(enabled && primary, |el| {
            el.bg(theme::button::BLUE_BG)
                .text_color(theme::button::BLUE_FG)
        })
        .when(enabled && !primary, |el| {
            el.bg(theme::surface::CHIP_NEUTRAL)
                .text_color(theme::text::HEADING)
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
        })
        .when(!enabled, |el| {
            el.bg(theme::surface::CHIP_NEUTRAL)
                .text_color(theme::text::GHOST)
        })
        .font(font(theme::font::SANS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(10.5))
        .child(label)
}

fn render_rebase_column_header() -> gpui::AnyElement {
    div()
        .id("rebase-column-header")
        .debug_selector(|| "rebase-column-header".to_string())
        .flex_none()
        .flex()
        .items_center()
        .gap(px(8.0))
        .px(px(10.0))
        .h(theme::graph::HEADER)
        .bg(theme::graph::HEADER_BG)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(theme::graph::HEADER_LABEL_FG)
        .child(div().w(px(14.0)).child(""))
        .child(div().w(px(14.0)).child(""))
        .child(div().w(px(76.0)).child("action"))
        .child(div().flex_1().child("commit"))
        .child(div().w(px(36.0)).child("files"))
        .child(div().w(px(56.0)).child("sha"))
        .child(div().w(px(18.0)).child(""))
        .into_any_element()
}

/// Design spec §1.5: outlined marker for a planned pause, filled for where the rebase actually
/// stopped. `edit`'s and a message-less `reword`'s planned/actual markers render identically -
/// the design spec itself says the visual distinction between the two `StopReason`s isn't
/// load-bearing.
fn render_rebase_pause_marker(planned: bool, actual: bool) -> gpui::AnyElement {
    if !planned && !actual {
        return gpui::Empty.into_any_element();
    }
    div()
        .w(px(8.0))
        .h(px(8.0))
        .rounded(theme::radius::CHIP)
        .when(actual, |el| el.bg(theme::status::ASK))
        .when(!actual, |el| el.border_1().border_color(theme::status::ASK))
        .into_any_element()
}

#[cfg(test)]
mod rebase_flow_tests {
    use super::rebase::RebasePhase;
    use crate::root::focus::palette_focus_tests;
    use crate::root::AdeApp;
    use crate::work_surface::agents::{AgentKind, ProcessKind};
    use gpui::{Entity, TestAppContext};
    use std::path::Path;
    use wt_core::rebase::RebaseOutcome;

    fn git(dir: &Path, args: &[&str]) {
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

    fn git_output(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(file), contents).expect("write file");
        git(dir, &["add", file]);
        git(dir, &["commit", "-m", message]);
    }

    /// Three real commits on `main` (`base`, `second`, `third`), clean working tree - the graph
    /// walks them newest first, so row 0 is `third`, row 1 is `second`, row 2 is `base`.
    fn open_seeded_graph(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        Entity<AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        commit(repo.path(), "a.txt", "1", "base");
        commit(repo.path(), "a.txt", "2", "second");
        commit(repo.path(), "a.txt", "3", "third");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        (repo, app, cx)
    }

    fn right_click(cx: &mut gpui::VisualTestContext, position: gpui::Point<gpui::Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
    }

    // --- Entering/leaving the mode via the row menu and Cancel --------------------------------

    #[gpui::test]
    fn entering_via_a_real_row_menu_click_builds_the_real_plan_and_cancel_leaves_it(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);

        // Real right-click on row 2 (`base`, the oldest commit) opens its row menu.
        let row = cx
            .debug_bounds("graph-row-2")
            .expect("row 2 must be painted");
        right_click(cx, row.center());

        let option = cx
            .debug_bounds("dropdown-menu-row-Interactive rebase from here")
            .expect("the real, now-enabled row must paint a clickable option");
        cx.simulate_click(option.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app
                .graph_state
                .rebase
                .as_ref()
                .expect("a real click must have entered rebase mode");
            assert!(matches!(rebase_state.phase, RebasePhase::Planning));
            let subjects: Vec<&str> = rebase_state
                .plan
                .iter()
                .map(|row| row.original_subject.as_str())
                .collect();
            assert_eq!(
                subjects,
                vec!["second", "third"],
                "the plan must be oldest-first, excluding the onto row (`base`) itself"
            );
        });

        let cancel = cx
            .debug_bounds("rebase-banner-button-Cancel")
            .expect("the real Cancel button must be painted");
        cx.simulate_click(cancel.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "a real click on Cancel must leave rebase mode"
            );
        });
    }

    // --- Changing a row's action via the chip menu ---------------------------------------------

    #[gpui::test]
    fn clicking_the_action_chip_and_an_option_changes_the_rows_real_action(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        let chip = cx
            .debug_bounds("rebase-action-chip-1")
            .expect("row 1's action chip must be painted");
        cx.simulate_click(chip.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        let option = cx
            .debug_bounds("rebase-action-option-1-squash")
            .expect("the squash option must be painted once the dropdown is open");
        cx.simulate_click(option.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app
                .graph_state
                .rebase
                .as_ref()
                .expect("still in rebase mode");
            assert_eq!(
                rebase_state.plan[1].action,
                super::rebase::RebaseActionKind::Squash,
                "a real click on the squash option must change row 1's real action"
            );
            assert!(
                rebase_state.action_menu_open.is_none(),
                "choosing an option must close the dropdown"
            );
            assert_eq!(
                super::rebase::derive_result_commit_count(&rebase_state.plan),
                1,
                "squashing row 1 into row 0 must fold the two into a single resulting commit"
            );
        });
    }

    // --- Typing a reword message updates N -> M and the Result panel live ----------------------

    #[gpui::test]
    fn typing_a_reword_message_live_updates_the_supplied_message_and_result_blocks(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Reword, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            assert!(!rebase_state.plan[0].has_supplied_reword_message());
            let blocks = super::rebase::derive_result_blocks(&rebase_state.plan);
            assert_eq!(
                blocks[0].status,
                super::rebase::ResultBlockStatus::StopsForMessage
            );
        });

        let focus_handle = app.read_with(cx, |app, _| {
            app.graph_state
                .rebase
                .as_ref()
                .expect("in rebase mode")
                .plan[0]
                .reword_focus_handle
                .clone()
        });
        app.update_in(cx, |_app, window, cx| {
            window.focus(&focus_handle, cx);
        });
        cx.simulate_input(" reworded live");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            assert!(
                rebase_state.plan[0].has_supplied_reword_message(),
                "a real keystroke into the focused reword field must supply a real message"
            );
            let blocks = super::rebase::derive_result_blocks(&rebase_state.plan);
            assert!(
                blocks[0].subject.contains("reworded live"),
                "the Result panel's derivation must reflect the live-typed text, got {:?}",
                blocks[0].subject
            );
            assert_eq!(blocks[0].status, super::rebase::ResultBlockStatus::Reworded);
        });
    }

    // --- Starting a plan that completes cleanly -------------------------------------------------

    #[gpui::test]
    fn starting_a_plan_that_drops_a_commit_completes_and_leaves_rebase_mode(
        cx: &mut TestAppContext,
    ) {
        // Independent files, unlike `open_seeded_graph`'s single shared file - dropping `second`
        // must not conflict with `third`, which touches a different file entirely.
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        commit(repo.path(), "base.txt", "base", "base");
        commit(repo.path(), "a.txt", "1", "second");
        commit(repo.path(), "b.txt", "1", "third");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Drop, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "a real Completed outcome must leave rebase mode"
            );
        });
        let subjects = git_output(repo.path(), &["log", "--format=%s", "--reverse"]);
        assert_eq!(
            subjects, "base\nthird",
            "the real rebase must have really dropped `second` from history"
        );
    }

    // --- Starting a plan that stops for edit ----------------------------------------------------

    #[gpui::test]
    fn starting_a_plan_that_stops_for_edit_shows_the_right_pause_marker(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Edit, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("stopped, not left");
            let stopped_commit = match &rebase_state.phase {
                RebasePhase::Stopped {
                    outcome: RebaseOutcome::StoppedForEdit { commit, .. },
                } => commit.clone(),
                other => panic!("expected StoppedForEdit, got a different phase: {other:?}"),
            };
            assert_eq!(
                stopped_commit, rebase_state.plan[0].commit,
                "the real stop must be reported at the edit row's own commit"
            );
            assert!(
                super::rebase::outcome_stopped_commit(match &rebase_state.phase {
                    RebasePhase::Stopped { outcome } => outcome,
                    RebasePhase::Planning => unreachable!(),
                }) == Some(rebase_state.plan[0].commit.as_str()),
                "the row-level filled pause marker must match the real stopped commit"
            );
        });
    }

    // --- A message-less reword stop, then Continue after supplying a message -------------------

    #[gpui::test]
    fn a_message_less_reword_stop_completes_once_continue_runs_after_a_real_message_is_supplied(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        // Row 1 (`third`) - the plan's own last/newest row, so it lands as the real final `HEAD`
        // once `continue_rebase` completes, letting this test check `HEAD`'s own subject directly
        // rather than having to dig a non-`HEAD` commit's message back out of the log.
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(1, super::rebase::RebaseActionKind::Reword, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("stopped");
            assert!(matches!(
                rebase_state.phase,
                RebasePhase::Stopped {
                    outcome: RebaseOutcome::StoppedForEdit {
                        reason: Some(wt_core::rebase::StopReason::RewordNeedsMessage),
                        ..
                    }
                }
            ));
        });

        // Supply a real message for real, exactly the way `Self::handle_rebase_reword_key_down`
        // would from a real keystroke - not fabricated state.
        app.update_in(cx, |app, _window, _cx| {
            let rebase_state = app.graph_state.rebase.as_mut().expect("stopped");
            rebase_state.plan[1]
                .reword_message
                .set("a real supplied message", std::time::Instant::now());
        });

        app.update_in(cx, |app, _window, cx| {
            app.continue_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "continuing after a real supplied message must run the reword through and complete"
            );
        });
        let head_subject = git_output(repo.path(), &["log", "-1", "--format=%s"]);
        assert_eq!(head_subject, "a real supplied message");
    }

    // --- A real conflict stop shows the stopped-state banner ------------------------------------

    #[gpui::test]
    fn a_real_conflict_stop_shows_the_banner_and_resolve_opens_the_real_file(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        commit(repo.path(), "file.txt", "base", "base");
        commit(repo.path(), "file.txt", "v1", "commit v1");
        commit(repo.path(), "file.txt", "v2", "commit v2");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();

        // Row 2 is `base`; entering from there gives a plan of `[commit v1, commit v2]`. Swapping
        // them (v2 applied before v1) forces a real conflict - v2's own diff assumes v1's content
        // as its parent, which base does not have.
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        let (c1, c2) = app.read_with(cx, |app, _| {
            let plan = &app
                .graph_state
                .rebase
                .as_ref()
                .expect("in rebase mode")
                .plan;
            (plan[0].commit.clone(), plan[1].commit.clone())
        });
        app.update_in(cx, |app, _window, _cx| {
            let rebase_state = app.graph_state.rebase.as_mut().expect("in rebase mode");
            super::rebase::move_rebase_plan_row(&mut rebase_state.plan, &c2, &c1, false);
        });

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app
                .graph_state
                .rebase
                .as_ref()
                .expect("stopped on a real conflict");
            assert!(matches!(
                rebase_state.phase,
                RebasePhase::Stopped {
                    outcome: RebaseOutcome::StoppedForConflict { .. }
                }
            ));
        });

        let strip = cx
            .debug_bounds("rebase-stopped-strip")
            .expect("the real Stopped-phase strip must paint for a real conflict");
        let link = cx
            .debug_bounds("rebase-resolve-conflict-link")
            .expect("the real resolve link must paint inside it");
        assert!(strip.contains(&link.origin));

        cx.simulate_click(link.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_change.as_deref(),
                Some(Path::new("file.txt")),
                "clicking Resolve in the diff view must route to this app's real existing file \
                 view, on the real conflicted path"
            );
        });
    }

    // --- Running-agent warning + real Pause now / resume ----------------------------------------

    #[gpui::test]
    fn running_agent_warning_shows_and_pause_now_really_suspends_the_process_with_resume_on_leave(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_graph(cx);

        let agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                window,
                cx,
            )
        });
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(agent_id, ProcessKind::from(AgentKind::Claude));
        });
        cx.run_until_parked();

        let pid = app.read_with(cx, |app, cx| {
            app.agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .expect("agent spawned")
                .pane
                .read(cx)
                .pid()
                .expect("a real shell process must have a real pid by now")
        });

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        let warning = cx.debug_bounds("rebase-pause-agents");
        assert!(
            warning.is_some(),
            "with a real running agent in this worktree, the Pause now warning must appear"
        );

        app.update_in(cx, |app, _window, cx| {
            app.pause_rebase_agents(cx);
        });
        cx.run_until_parked();

        wait_for_proc_state(pid, "T");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .rebase
                    .as_ref()
                    .expect("in rebase mode")
                    .paused_agents,
                vec![agent_id]
            );
        });

        app.update_in(cx, |app, _window, cx| {
            app.cancel_rebase_mode(cx);
        });
        cx.run_until_parked();

        wait_for_proc_state_not(pid, "T");
    }

    fn worktree_item(
        path: std::path::PathBuf,
        label: &str,
    ) -> crate::rail::worktrees::WorktreeItem {
        crate::rail::worktrees::WorktreeItem {
            path,
            label: label.to_string(),
            branch: Some(label.to_string()),
            is_main: false,
            is_bare: false,
            is_detached: false,
            short_sha: None,
            is_locked: false,
            lock_reason: None,
            is_broken: false,
            broken_reason: None,
            error: None,
        }
    }

    /// GitHub issue #242 phase B review fix #1 - real, independently reproduced: switching
    /// worktrees in the rail while rebase mode was live used to leave `graph_state.rebase`
    /// (and any paused agent) pointed at the worktree that had just stopped being
    /// `self.diff_root`, so every subsequent click silently ran real git against the wrong
    /// worktree. `Self::select_worktree` -> `Self::reset_repo_scoped_state` must leave the mode
    /// outright (real agent resume included) the instant that switch happens.
    #[gpui::test]
    fn switching_worktrees_while_rebase_mode_is_live_leaves_the_mode_and_resumes_paused_agents(
        cx: &mut TestAppContext,
    ) {
        let (repo_a, app, cx) = open_seeded_graph(cx);

        let repo_b = tempfile::tempdir().expect("tempdir b");
        git(repo_b.path(), &["init", "-b", "main"]);
        git(repo_b.path(), &["config", "user.email", "test@example.com"]);
        git(repo_b.path(), &["config", "user.name", "Test User"]);
        commit(repo_b.path(), "b.txt", "1", "repo b base");

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(repo_a.path().to_path_buf(), "repo-a"),
                worktree_item(repo_b.path().to_path_buf(), "repo-b"),
            ];
        });

        let agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo_a.path().to_path_buf(),
                12.0,
                None,
                window,
                cx,
            )
        });
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(agent_id, ProcessKind::from(AgentKind::Claude));
        });
        cx.run_until_parked();
        let pid = app.read_with(cx, |app, cx| {
            app.agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .expect("agent spawned")
                .pane
                .read(cx)
                .pid()
                .expect("a real shell process must have a real pid by now")
        });

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.pause_rebase_agents(cx);
        });
        cx.run_until_parked();
        wait_for_proc_state(pid, "T");
        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_some(),
                "sanity check: still in rebase mode before the switch"
            );
        });

        // The real rail gesture: select the *other* worktree.
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "switching worktrees while rebase mode was live must leave the mode outright"
            );
        });
        wait_for_proc_state_not(pid, "T");
    }

    /// GitHub issue #242 phase B review fix #2 - real, independently reproduced: `Continue`/
    /// `Skip` had no `op_in_flight` guard, so a second click (here, a real `Skip` fired right
    /// after a real `Continue`, both before the first's own background call resolves) could run
    /// concurrently with the first's still-in-flight real `wt_core::rebase` call. Proven here by
    /// a real, materially different final result if the guard is missing: an unguarded `Skip`
    /// racing a real `Continue` from an `edit` stop would really drop the plan's next commit from
    /// history; with the guard, `Skip` is refused outright and the plan completes with every real
    /// commit intact.
    #[gpui::test]
    fn a_second_op_click_while_the_first_is_still_in_flight_is_refused_not_raced(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Edit, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(matches!(
                app.graph_state.rebase.as_ref().expect("stopped").phase,
                RebasePhase::Stopped {
                    outcome: RebaseOutcome::StoppedForEdit { .. }
                }
            ));
        });

        // Both calls happen before either background call has a chance to resolve - the exact
        // double-click race the review reproduced.
        app.update_in(cx, |app, _window, cx| {
            app.continue_rebase(cx);
            app.skip_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "the guarded Continue must have completed the plan for real"
            );
        });
        let subjects = git_output(repo.path(), &["log", "--format=%s", "--reverse"]);
        assert_eq!(
            subjects, "base\nsecond\nthird",
            "a raced, unguarded Skip would have really dropped `third` from history - the guard \
             must have refused it outright, leaving every real commit intact"
        );
    }

    /// GitHub issue #242 phase B review fix #3 - real, independently reproduced: `Cancel` had no
    /// `op_in_flight` guard at all, so clicking it while `Start rebase`'s real subprocess was
    /// still running dropped `graph_state.rebase` out from under it, leaving the repository
    /// genuinely mid-rebase with no banner left to recover it from. Proven here: a real `Cancel`
    /// click fired immediately after `Start`, before its own background call resolves, must be a
    /// no-op - the real outcome still lands and rebase mode is left through the genuine
    /// `Completed` path, not silently discarded.
    #[gpui::test]
    fn cancel_is_refused_while_start_rebase_is_in_flight_and_the_real_outcome_still_lands(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        // A plan that genuinely *stops* (not one that completes cleanly) is the scenario that
        // actually exposes the bug: if `Cancel` is allowed to discard `graph_state.rebase`
        // while `Start rebase`'s subprocess is still running, the repository still really ends
        // up stopped mid-rebase on disk (`.git/rebase-merge/` real and present) once that
        // subprocess finishes, but `apply_rebase_outcome`'s non-`Completed` arm has nothing left
        // to write the real `RebaseOutcome::StoppedForEdit` into (`graph_state.rebase` is
        // already `None`) - the real stop is silently dropped, with no banner and no in-app way
        // to `Abort`/`Continue`/`Skip` it. A plan that completes cleanly can't tell the two
        // apart (the real git operation runs to completion either way, regardless of what the
        // UI-side state shows), which is exactly why that shape doesn't belong in this test.
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Edit, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
            // Fired in the exact same synchronous window, before the background call above has
            // any chance to resolve.
            app.cancel_rebase_mode(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect(
                "the guarded Cancel must have been refused, leaving the real stop for \
                         the mode to receive and show - not abandoned",
            );
            assert!(
                matches!(
                    rebase_state.phase,
                    RebasePhase::Stopped {
                        outcome: RebaseOutcome::StoppedForEdit { .. }
                    }
                ),
                "expected the real StoppedForEdit outcome to have landed, got {:?}",
                rebase_state.phase
            );
        });
        // The real, on-disk rebase must still be genuinely recoverable - proven by actually
        // recovering it for real, through the same `Abort` a stuck user would reach for.
        app.update_in(cx, |app, _window, cx| {
            app.abort_rebase(cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.graph_state.rebase.is_none());
        });
        let subjects = git_output(repo.path(), &["log", "--format=%s", "--reverse"]);
        assert_eq!(
            subjects, "base\nsecond\nthird",
            "abort must have restored the exact pre-rebase history"
        );
    }

    /// GitHub issue #242 phase B review fix #5(a) - real, independently reproduced: closing the
    /// graph tab outright while rebase mode was live used to drop `graph_state.rebase` (and any
    /// paused agent) directly, with no real resume - the only surface that could trigger one had
    /// just been removed.
    #[gpui::test]
    fn closing_the_graph_tab_mid_rebase_resumes_a_paused_agent(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded_graph(cx);

        let agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                window,
                cx,
            )
        });
        app.update(cx, |app, _cx| {
            app.agents
                .set_kind_for_test(agent_id, ProcessKind::from(AgentKind::Claude));
        });
        cx.run_until_parked();
        let pid = app.read_with(cx, |app, cx| {
            app.agents
                .iter()
                .find(|agent| agent.id == agent_id)
                .expect("agent spawned")
                .pane
                .read(cx)
                .pid()
                .expect("a real shell process must have a real pid by now")
        });

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.pause_rebase_agents(cx);
        });
        cx.run_until_parked();
        wait_for_proc_state(pid, "T");

        app.update_in(cx, |app, window, cx| {
            app.close_git_graph_tab(window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.graph_state.rebase.is_none());
        });
        wait_for_proc_state_not(pid, "T");
    }

    #[gpui::test]
    fn no_running_agents_means_no_warning(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("rebase-pause-agents").is_none(),
            "with no running agent, the Pause now warning must not render at all"
        );
    }

    #[cfg(target_os = "linux")]
    fn proc_state(pid: u32) -> String {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
            .expect("reading /proc/<pid>/status should succeed while the process is alive");
        status
            .lines()
            .find_map(|line| line.strip_prefix("State:"))
            .map(|rest| rest.trim().to_string())
            .expect("State: line should be present")
    }

    #[cfg(target_os = "linux")]
    fn wait_for_proc_state(pid: u32, prefix: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = proc_state(pid);
            if state.starts_with(prefix) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process {pid} never reached the real kernel-reported state {prefix:?} - last \
                 observed: {state:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(target_os = "linux")]
    fn wait_for_proc_state_not(pid: u32, prefix: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let state = proc_state(pid);
            if !state.starts_with(prefix) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process {pid} never left the real kernel-reported state {prefix:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn wait_for_proc_state(_pid: u32, _prefix: &str) {}
    #[cfg(not(target_os = "linux"))]
    fn wait_for_proc_state_not(_pid: u32, _prefix: &str) {}
}
