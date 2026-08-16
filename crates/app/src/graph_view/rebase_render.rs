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
use crate::root::plural;
use crate::root::widgets::{SimpleInput, SimpleInputCaret, TextFieldHandle};
use gpui::{DragMoveEvent, KeyDownEvent, SharedString};

/// One interactive-rebase plan row's `reword` message field handle - what click/drag selection
/// and GitHub issue #336's four clipboard/select-all actions act on. `None` whenever the plan no
/// longer has a row at `index`, which is exactly the case [`TextFieldHandle`]'s own `Option`
/// exists for.
fn rebase_reword_handle(index: usize) -> TextFieldHandle {
    TextFieldHandle::new(move |app: &mut AdeApp| {
        app.graph_state
            .rebase
            .as_mut()
            .and_then(|state| state.plan.get_mut(index))
            .map(|row| &mut row.reword_message)
    })
}
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

/// Design spec §1.4's footer hint strip, verbatim and in order:
/// `alt+↑↓ reorder · P pick · S squash · D drop · mod+enter start`.
const REBASE_FOOTER_HINTS: [(&str, &str); 5] = [
    ("alt+\u{2191}\u{2193}", "reorder"),
    ("P", "pick"),
    ("S", "squash"),
    ("D", "drop"),
    ("mod+enter", "start"),
];

/// Which of design spec §1.7's three warnings a stack row is - and, through [`Self::dot`], the
/// severity it renders at. GitHub issue #305: all three used to render `theme::status::ASK`
/// regardless of what they meant, which flattened the gradient §1.7 is explicit about (the
/// running-agent warning is "the one no other git client needs"; the stop count is a neutral
/// fact) into three identical attention-amber rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RebaseWarningKind {
    /// §1.7 warning 1 - agents are really running in this worktree.
    RunningAgents,
    /// §1.7 warning 2 - some of the plan's commits are already on the tracked remote.
    RemoteCommits,
    /// §1.7 warning 3 - how many times the plan will hand control back.
    StopCount,
}

impl RebaseWarningKind {
    /// A stable, human-readable tag for this row's `debug_selector`s.
    fn slug(self) -> &'static str {
        match self {
            RebaseWarningKind::RunningAgents => "agents",
            RebaseWarningKind::RemoteCommits => "remote",
            RebaseWarningKind::StopCount => "stops",
        }
    }

    /// Design spec §1.7's own per-warning hue, verbatim: amber `#e2a336` · blue `#8fbde6` ·
    /// grey `#565d64`. Every one is an already-named token whose meaning matches (attention,
    /// informational, idle) - see [`rebase_warning_severity_tests`], which pins both the hexes
    /// and the fact that no two of the three are the same colour.
    pub(crate) fn dot(self) -> theme::ColorToken {
        match self {
            RebaseWarningKind::RunningAgents => theme::status::ASK,
            RebaseWarningKind::RemoteCommits => theme::graph::REBASE_WARNING_REMOTE,
            RebaseWarningKind::StopCount => theme::status::IDLE,
        }
    }
}

/// One action's chip colours - design spec §1.4's action table, verbatim. See
/// [`rebase_action_style`].
pub(crate) struct RebaseActionStyle {
    pub fg: theme::ColorToken,
    pub bg: theme::ColorToken,
    pub border: theme::ColorToken,
}

/// The single place design spec §1.4's action palette is decided (GitHub issue #302). Every
/// surface that colours by action reads it - the chip, its dropdown option's label, and §1.4's
/// fold elbow, whose stroke is specified as "a 1px elbow **in the action's colour**" - so those
/// three can never drift into disagreeing about what `squash` looks like.
pub(crate) fn rebase_action_style(action: RebaseActionKind) -> RebaseActionStyle {
    let (fg, bg, border) = match action {
        RebaseActionKind::Pick => (
            theme::graph::REBASE_PICK_FG,
            theme::graph::REBASE_PICK_BG,
            theme::graph::REBASE_PICK_BORDER,
        ),
        RebaseActionKind::Reword => (
            theme::graph::REBASE_REWORD_FG,
            theme::graph::REBASE_REWORD_BG,
            theme::graph::REBASE_REWORD_BORDER,
        ),
        RebaseActionKind::Edit => (
            theme::graph::REBASE_EDIT_FG,
            theme::graph::REBASE_EDIT_BG,
            theme::graph::REBASE_EDIT_BORDER,
        ),
        RebaseActionKind::Squash => (
            theme::graph::REBASE_SQUASH_FG,
            theme::graph::REBASE_SQUASH_BG,
            theme::graph::REBASE_SQUASH_BORDER,
        ),
        RebaseActionKind::Fixup => (
            theme::graph::REBASE_FIXUP_FG,
            theme::graph::REBASE_FIXUP_BG,
            theme::graph::REBASE_FIXUP_BORDER,
        ),
        RebaseActionKind::Drop => (
            theme::graph::REBASE_DROP_FG,
            theme::graph::REBASE_DROP_BG,
            theme::graph::REBASE_DROP_BORDER,
        ),
    };
    RebaseActionStyle { fg, bg, border }
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
            // GitHub issue #304: the one real key context behind design spec §1.4's footer
            // keycaps. Every binding scoped to it is registered in `crate::default_key_bindings`
            // (see that function's own docs for why four of the five also carry `&& !text-input`,
            // and `crate::keymap_overrides::real_context_stacks` for the two live stacks this
            // node produces).
            .key_context("rebase-plan")
            .on_action(cx.listener(
                |this, _action: &crate::root::RebaseReorderUp, _window, cx| {
                    this.move_selected_rebase_plan_row(true, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _action: &crate::root::RebaseReorderDown, _window, cx| {
                    this.move_selected_rebase_plan_row(false, cx);
                },
            ))
            .on_action(
                cx.listener(|this, _action: &crate::root::RebasePickRow, _window, cx| {
                    this.set_selected_rebase_row_action(RebaseActionKind::Pick, cx);
                }),
            )
            .on_action(cx.listener(
                |this, _action: &crate::root::RebaseSquashRow, _window, cx| {
                    this.set_selected_rebase_row_action(RebaseActionKind::Squash, cx);
                },
            ))
            .on_action(
                cx.listener(|this, _action: &crate::root::RebaseDropRow, _window, cx| {
                    this.set_selected_rebase_row_action(RebaseActionKind::Drop, cx);
                }),
            )
            // The keyboard twin of the `Start rebase` button, guarded exactly the way that
            // button is: `start_rebase` itself no-ops while `op_in_flight`, so a held
            // `mod+enter` can never launch a second overlapping git subprocess.
            .on_action(
                cx.listener(|this, _action: &crate::root::RebaseStart, _window, cx| {
                    if this
                        .graph_state
                        .rebase
                        .as_ref()
                        .is_some_and(|state| matches!(state.phase, RebasePhase::Planning))
                    {
                        this.start_rebase(cx);
                    }
                }),
            )
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
            .child(self.render_rebase_plan_footer())
            .into_any_element()
    }

    /// Design spec §1.4's **Footer**, 28 high: the keycap hints, a flex spacer, §1.5's
    /// `pauses here` legend, then §1.1's order note.
    fn render_rebase_plan_footer(&self) -> gpui::AnyElement {
        let macos = self.window_controls_style().is_macos();
        div()
            .id("rebase-plan-footer")
            .debug_selector(|| "rebase-plan-footer".to_string())
            .flex_none()
            .h(theme::graph::REBASE_FOOTER)
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER)
            .child(crate::root::widgets::render_hint_row(
                REBASE_FOOTER_HINTS.iter().map(|(spec, label)| {
                    crate::root::widgets::render_hint_pair(
                        &crate::keymap::resolve_combo(spec, macos),
                        *label,
                    )
                    .into_any_element()
                }),
            ))
            .child(div().flex_1())
            .child(
                div()
                    .id("rebase-pause-legend")
                    .debug_selector(|| "rebase-pause-legend".to_string())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    // §1.5's own outlined mark, not a second glyph for the same idea - "one
                    // column, one legend".
                    .child(render_rebase_pause_marker(true, false))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(10.0))
                            .text_color(theme::text::PATH)
                            .child("pauses here"),
                    ),
            )
            .child(
                div()
                    .id("rebase-order-note")
                    .debug_selector(|| "rebase-order-note".to_string())
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::HINT)
                    .child("oldest first \u{b7} applied top to bottom"),
            )
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
                            .child(format!("{n} \u{2192} {}", plural::count(m, "commit", None))),
                    )
                    .child(
                        render_rebase_button("Cancel", RebaseButtonStyle::Ghost, enabled, &[])
                            .when(enabled, |el| {
                                el.on_click(cx.listener(
                                    |this, _event: &ClickEvent, _window, cx| {
                                        this.cancel_rebase_mode(cx);
                                    },
                                ))
                            }),
                    )
                    // GitHub issue #304: the real `mod+enter` keycaps §1.2 asks for, behind the
                    // real `crate::root::RebaseStart` binding - not a painted-on hint.
                    .child(
                        render_rebase_button(
                            "Start rebase",
                            RebaseButtonStyle::Primary,
                            enabled,
                            &crate::keymap::resolve_combo(
                                "mod+enter",
                                self.window_controls_style().is_macos(),
                            ),
                        )
                        .when(enabled, |el| {
                            el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.start_rebase(cx);
                            }))
                        }),
                    )
                    .into_any_element()
            }
            RebasePhase::Stopped { .. } => div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    render_rebase_button("Abort", RebaseButtonStyle::GhostDanger, enabled, &[])
                        .when(enabled, |el| {
                            el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.abort_rebase(cx);
                            }))
                        }),
                )
                .child(
                    render_rebase_button("Skip", RebaseButtonStyle::Ghost, enabled, &[]).when(
                        enabled,
                        |el| {
                            el.on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.skip_rebase(cx);
                            }))
                        },
                    ),
                )
                .child(
                    render_rebase_button("Continue", RebaseButtonStyle::Confirm, enabled, &[])
                        .when(enabled, |el| {
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
                // §1.8's own 5px amber square, the same mark §1.5's filled pause marker paints -
                // one vocabulary for "the rebase handed control back here".
                .child(
                    div()
                        .debug_selector(|| "rebase-stopped-strip-mark".to_string())
                        .flex_none()
                        .w(theme::graph::REBASE_MARK)
                        .h(theme::graph::REBASE_MARK)
                        .rounded(theme::radius::MARK_SM)
                        .bg(theme::status::ASK),
                )
                .child(
                    div()
                        .debug_selector(|| "rebase-stopped-strip-text".to_string())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight(450.0))
                        .text_size(px(11.0))
                        // GitHub issue #305: amber `#c99b4e`, not `status::FAIL`'s red. A rebase
                        // stopping on a conflict is "needs input", which is exactly what this
                        // app's amber is reserved for (see `theme::agent`'s reserved-hue rule);
                        // the red it used to wear is reserved for "failure and deletions" and
                        // said the rebase had broken rather than that it is waiting.
                        .text_color(theme::status::ASK_CARD_FG)
                        .child(format!(
                            "stopped at {stopped_at} of {total} \u{b7} {} in {file_label}",
                            plural::count(conflicted_files.len(), "conflict", None)
                        )),
                )
                .child(
                    div()
                        .id("rebase-resolve-conflict-link")
                        .debug_selector(|| "rebase-resolve-conflict-link".to_string())
                        .flex_none()
                        .cursor_pointer()
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight(450.0))
                        .text_size(px(10.5))
                        .text_color(theme::text::SECONDARY)
                        .hover(|el| el.text_color(theme::text::SELECTED))
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
            // Design spec §2.2's rebase menu anchor formula (`44 + 22 + 3 + row × 28 + 30`) counts
            // this 3 as the list's own top padding - see `Self::render_rebase_action_menu`.
            .py(theme::graph::REBASE_LIST_PAD)
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
        let is_selected = rebase_state.selected_index() == Some(index);
        let action_style = rebase_action_style(row.action);
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
            // Design spec §1.4: 28 high, no separator rule (the plan is one continuous list, not
            // the graph's own ruled commit rows), and no horizontal padding of its own - the 2px
            // selection edge plus the 11-wide drag-handle slot is exactly §1.3's "13 left pad,
            // clears the rows' 2px selection edge" on the `action` column header above it.
            .h(theme::graph::REBASE_ROW)
            // Always painted, transparent when unselected: a border that only exists on the
            // selected row would shift every other row 2px sideways the moment selection moved -
            // the exact bug `crate::rail::render`'s agent rows were fixed for in GitHub issue
            // #289.
            .border_l_2()
            .border_color(if is_selected {
                theme::border::SELECTED_EDGE.into()
            } else {
                work_surface::TRANSPARENT
            })
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_rebase_plan_row(index, cx);
            }))
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
            // Design spec §1.4: "drag handle | 11 wide, `⋮⋮` 9px mono `#363b40`".
            .child(
                div()
                    .flex_none()
                    .w(theme::graph::REBASE_DRAG_SLOT)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_grab()
                    .font(font(theme::font::MONO))
                    .text_size(px(9.0))
                    .text_color(theme::graph::REBASE_DRAG_HANDLE)
                    .child("\u{22ee}\u{22ee}"),
            )
            .child(render_rebase_fold_elbow(index, is_folded, action_style.fg))
            .child(self.render_rebase_action_chip(index, row.action, cx))
            .child(if is_reword {
                self.render_rebase_reword_field(index, row, cx)
            } else {
                div()
                    .flex_1()
                    .min_w_0()
                    .ml(px(9.0))
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    // §1.4's subject ramp, in the design's own precedence: dropped wins over
                    // folded wins over selected wins over normal.
                    .text_color(if is_dropped {
                        theme::text::GHOST
                    } else if is_folded {
                        theme::text::DIM
                    } else if is_selected {
                        theme::text::SELECTED
                    } else {
                        theme::text::STRONG
                    })
                    .when(is_dropped, |el| el.line_through())
                    .child(row.original_subject.clone())
                    .into_any_element()
            })
            .child(
                div()
                    .flex_none()
                    .w(theme::graph::REBASE_COL_NUMERIC)
                    .text_right()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(if is_dropped {
                        theme::text::DISABLED
                    } else {
                        theme::text::PATH
                    })
                    .child(files_label),
            )
            .child(
                div()
                    .flex_none()
                    .w(theme::graph::REBASE_COL_NUMERIC)
                    .text_right()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(if is_dropped {
                        theme::text::DISABLED
                    } else {
                        theme::text::FAINTER
                    })
                    .child(row.short_sha.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .w(theme::graph::REBASE_COL_PAUSE)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(render_rebase_pause_marker(planned_pause, actual_stop)),
            )
            .when(action_menu_open, |el| {
                el.child(self.render_rebase_action_menu(index, row.action, cx))
            })
            .into_any_element()
    }

    fn render_rebase_action_chip(
        &self,
        index: usize,
        action: RebaseActionKind,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let style = rebase_action_style(action);
        div()
            .id(("rebase-action-chip", index))
            .debug_selector(move || format!("rebase-action-chip-{index}"))
            .flex_none()
            // Design spec §1.4: "18 high, `0 7` padding, radius 3, `<action> ▾`". Deliberately
            // **not** a fixed width: the chip is sized by its own label, which is what makes the
            // six of them read as six different things rather than one column of interchangeable
            // boxes (GitHub issue #302).
            .h(theme::graph::REBASE_CHIP)
            .px(px(7.0))
            .rounded(theme::radius::CHIP)
            .bg(style.bg)
            .border_1()
            .border_color(style.border)
            .hover(|el| el.border_color(theme::graph::REBASE_CHIP_HOVER_BORDER))
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(5.0))
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.0))
            .text_color(style.fg)
            .child(action.label())
            .child(
                div()
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_size(px(8.0))
                    .text_color(theme::text::FAINTER)
                    .child("\u{25be}"),
            )
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                // The chip selects its own row before opening the menu - otherwise the keyboard
                // actions in §1.4's footer would still be pointed at whatever row was selected
                // before, while the user is visibly working on this one.
                this.select_rebase_plan_row(index, cx);
                this.toggle_rebase_action_menu(index, cx);
            }))
            .into_any_element()
    }

    /// The action chip's own dropdown - real `pick`/`reword`/`edit`/`squash`/`fixup`/`drop`
    /// options with design spec §1.4's own hints, positioned by a plain `.relative()`/
    /// `.absolute()` nesting inside its own row (unlike the row `⋯`/Push menus, this needs no
    /// window-space bounds: its anchor is a fixed offset from the row it belongs to, so it
    /// follows that row for free when the plan list scrolls - see
    /// `crate::graph_view::render::render_graph_row_menu`'s own docs for why *that* popover needs
    /// the heavier window-space mechanism instead).
    fn render_rebase_action_menu(
        &self,
        index: usize,
        current: RebaseActionKind,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut menu = div()
            .id(("rebase-action-menu", index))
            .debug_selector(move || format!("rebase-action-menu-{index}"))
            .absolute()
            // Design spec §1.4's own anchor: `left:16`, `top = 44 + 22 + 3 + row × 28 + 30`.
            // Expressed relative to the row (which this menu is a child of) the pane-space
            // formula's first four terms *are* the row's own top - `44` banner + `22` column
            // header + `3` list padding + `row × 28` - so what is left is the trailing `+ 30`,
            // i.e. one row height plus 2. Stated as arithmetic on the same constants rather than
            // as a literal 30, so a row-height change can never silently put the menu back on top
            // of the row it belongs to (§1.4: "verify it never overlaps the row it belongs to").
            .top(theme::graph::REBASE_ROW + px(2.0))
            .left(px(16.0))
            .w(theme::graph::REBASE_MENU_WIDTH)
            .py(px(4.0))
            .occlude()
            .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
            }));
        menu = crate::root::widgets::menu_popover_chrome(menu, theme::shadow::MENU);
        for kind in RebaseActionKind::ALL {
            let style = rebase_action_style(kind);
            let is_current = kind == current;
            menu = menu.child(
                div()
                    .id(format!("rebase-action-option-{index}-{}", kind.label()))
                    .debug_selector(move || {
                        format!("rebase-action-option-{index}-{}", kind.label())
                    })
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(5.0))
                    .cursor_pointer()
                    // §1.4: "current action highlighted `#1d2226`" - the same fill this menu's
                    // own hover uses, deliberately: "the row you are on" and "the row you would
                    // land on" are one visual idea here.
                    .when(is_current, |el| el.bg(theme::surface::MENU_ROW_HOVER))
                    .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
                    // §1.4: "Rows are ✓ mark 9 · action name 46 in 10.5px/500 mono · hint flex
                    // in 10px Plex Sans `#5e646a`".
                    .child(
                        div()
                            .flex_none()
                            .w(theme::graph::REBASE_MENU_MARK)
                            .font(font(theme::font::MONO))
                            .text_size(px(9.0))
                            .text_color(theme::graph::REBASE_SQUASH_FG)
                            .child(if is_current { "\u{2713}" } else { "" }),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(theme::graph::REBASE_MENU_NAME)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(10.5))
                            .text_color(style.fg)
                            .child(kind.label()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font(font(theme::font::SANS))
                            .text_size(px(10.0))
                            .text_color(theme::text::FAINTER)
                            .child(kind.hint()),
                    )
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.set_rebase_row_action(index, kind, cx);
                    })),
            );
        }
        gpui::deferred(menu).with_priority(1).into_any_element()
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
        let supplied = row.has_supplied_reword_message();
        let field = rebase_reword_handle(index);
        let row_node = div()
            .id(("rebase-reword-field", index))
            .flex_1()
            .min_w_0()
            .ml(px(9.0))
            .track_focus(&focus_handle)
            .key_context("text-input")
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.handle_rebase_reword_key_down(index, event, window, cx);
            }));
        // GitHub issue #336's four clipboard/select-all actions, on the same node that carries
        // this row's `"text-input"` word.
        self.wire_text_input_actions(row_node, field.clone(), cx)
            // §1.4: "Its click must stop propagating, so placing the caret does not fight row
            // selection" - but the row still becomes the selected one, since that is visibly the
            // row being worked on and is what §1.4's keyboard actions then act upon.
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                cx.stop_propagation();
                this.select_rebase_plan_row(index, cx);
            }))
            // §1.6: 21 high, `0 7` padding, radius 3, 1px border that turns from `#3b4a58` to
            // `#2b3d4f` once a message is really supplied - the same "supplied" predicate the
            // pause column and the stop count already read (`has_supplied_reword_message`), so
            // the field's own border can never disagree with them.
            .h(px(21.0))
            .px(px(7.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::CARD_SUNK)
            .border_1()
            .border_color(if supplied {
                theme::graph::REBASE_REWORD_BORDER
            } else {
                theme::graph::REBASE_REWORD_BORDER_EMPTY
            })
            .flex()
            .items_center()
            // GitHub issue #336: through the one helper that owns this structure, like every
            // other simple input in the app. Before this, the caret was pinned after the text
            // whatever the row's own `TextField::caret` said, and there was no selection
            // highlight or click hit-testing at all.
            .child(self.render_simple_input_row(
                SimpleInput {
                    caret_selector: SharedString::from(format!("rebase-reword-caret-{index}")),
                    text_selector: SharedString::from(format!("rebase-reword-text-{index}")),
                    focus_handle: Some(&focus_handle),
                    text: &text,
                    caret_offset: row.reword_message.caret(),
                    selection: row.reword_message.selection(),
                    placeholder: "",
                    font: theme::font::SANS,
                    text_size: px(11.5),
                    text_color: theme::text::SELECTED,
                    placeholder_color: theme::text::GHOST,
                    caret: SimpleInputCaret::default(),
                    field: Some(field),
                },
                cx,
            ))
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
                                "{} \u{2192} {}",
                                rebase_state.plan.len(),
                                plural::count(count, "commit", None)
                            )),
                    ),
            );

        for block in &blocks {
            panel = panel.child(render_rebase_result_block(block));
        }

        panel = panel.child(div().flex_1());

        // Design spec §1.7: the warning stack is **pinned at the foot** as one band of its own -
        // a single `border-top` above the whole stack, then one bottom-ruled row per warning, not
        // three independently top-bordered strips stacked up.
        let mut warnings = div()
            .id("rebase-warning-stack")
            .debug_selector(|| "rebase-warning-stack".to_string())
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(theme::border::INNER)
            .bg(theme::surface::FOOTER);
        let mut any = false;
        if let Some(warning) = self.render_rebase_agent_warning(rebase_state.op_in_flight, cx) {
            warnings = warnings.child(warning);
            any = true;
        }
        if let Some(warning) = render_rebase_remote_warning(rebase_state) {
            warnings = warnings.child(warning);
            any = true;
        }
        if let Some(warning) = render_rebase_stop_count_warning(rebase_state) {
            warnings = warnings.child(warning);
            any = true;
        }
        if any {
            panel = panel.child(warnings);
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
            render_rebase_warning_shell(
                RebaseWarningKind::RunningAgents,
                format!(
                    "{} {} running here",
                    plural::count(running, "agent", None),
                    plural::form(running, "is", "are"),
                ),
                format!(
                    "A rebase rewrites the files under {}. Jerry pauses {} for the rebase and \
                     resumes {} after.",
                    plural::form(running, "it", "them"),
                    plural::form(running, "it", "them"),
                    plural::form(running, "it", "them"),
                ),
            )
            .child(
                div()
                    .id("rebase-pause-agents")
                    .debug_selector(|| "rebase-pause-agents".to_string())
                    .flex_none()
                    .when(enabled, |el| el.cursor_pointer())
                    .when(!enabled, |el| el.cursor_default().opacity(0.5))
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight(450.0))
                    .text_size(px(10.5))
                    // §1.7: `Pause now` in `#c99b4e` - a plain text action, not a filled chip.
                    .text_color(theme::status::ASK_CARD_FG)
                    .when(enabled, |el| {
                        el.hover(|el| el.text_color(theme::text::SELECTED))
                            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                                this.pause_rebase_agents(cx);
                            }))
                    })
                    .child("Pause now"),
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
                        (n, Some(status)) => format!(
                            "{} folded in \u{b7} {status}",
                            plural::count(n, "commit", None)
                        ),
                        (n, None) => format!("{} folded in", plural::count(n, "commit", None)),
                    }),
            )
        })
        .into_any_element()
}

/// Design spec §1.7 warning 2 - **blue**, not amber: nothing is wrong, the plan simply has a
/// consequence for the remote afterwards. It is the informational step of the stack's severity
/// gradient (GitHub issue #305), which flattened entirely while all three rendered attention
/// amber.
fn render_rebase_remote_warning(rebase_state: &RebaseModeState) -> Option<gpui::AnyElement> {
    let count = rebase_state.already_on_upstream.len();
    if count == 0 {
        return None;
    }
    let total = rebase_state.plan.len();
    Some(
        render_rebase_warning_shell(
            RebaseWarningKind::RemoteCommits,
            format!(
                "{count} of {} {} on the remote",
                plural::count(total, "commit", None),
                plural::form(count, "is", "are"),
            ),
            format!(
                "origin/{} will need a force-with-lease push afterwards.",
                rebase_state.branch
            ),
        )
        .into_any_element(),
    )
}

/// Design spec §1.7 warning 3 - **grey**. A stop count is a fact about the plan, not a problem
/// with it, and §1.6's whole argument is that the user can drive it to zero by typing the reword
/// messages up front. Rendering it amber (as it used to) made a neutral count look exactly like
/// the running-agent warning sitting above it.
fn render_rebase_stop_count_warning(rebase_state: &RebaseModeState) -> Option<gpui::AnyElement> {
    let n = derive_stop_count(&rebase_state.plan);
    if n == 0 {
        return None;
    }
    Some(
        render_rebase_warning_shell(
            RebaseWarningKind::StopCount,
            format!("Stops {}", plural::count(n, "time", None)),
            "edit always stops. A reword only stops if you leave its message alone - type it \
             here and the rebase runs through."
                .to_string(),
        )
        .into_any_element(),
    )
}

/// Design spec §1.7's warning row, verbatim: "one row each (5px square · title 11px/450 `#c2c7cc`
/// · body 10.5px/15 `#767d84` · optional action)".
fn render_rebase_warning_shell(kind: RebaseWarningKind, title: String, body: String) -> gpui::Div {
    let slug = kind.slug();
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(9.0))
        .border_b_1()
        .border_color(theme::border::ROW)
        .child(
            div()
                .debug_selector(move || format!("rebase-warning-{slug}-dot"))
                .flex_none()
                .w(theme::graph::REBASE_MARK)
                .h(theme::graph::REBASE_MARK)
                .mt(px(4.0))
                .rounded(theme::radius::MARK_SM)
                .bg(kind.dot()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .debug_selector(move || format!("rebase-warning-{slug}-title"))
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight(450.0))
                        .text_size(px(11.0))
                        .text_color(theme::text::STRONG)
                        .child(title),
                )
                .child(
                    div()
                        .debug_selector(move || format!("rebase-warning-{slug}-body"))
                        .font(font(theme::font::SANS))
                        .text_size(px(10.5))
                        .line_height(px(15.0))
                        .text_color(theme::graph::REBASE_WARNING_BODY)
                        .child(body),
                ),
        )
}

/// Which of design spec §1.2's three banner treatments a button wears.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RebaseButtonStyle {
    /// `Cancel`/`Skip` - a plain ghost, hover-filled only.
    Ghost,
    /// `Abort` - the same ghost, in the danger red §1.2 gives it. A destructive verb rendered
    /// exactly like `Skip` was real drift: the two do opposite things to the user's history.
    GhostDanger,
    /// `Start rebase` - §1.2's green primary, "with `mod+enter` keycaps in the button's own
    /// border colour".
    Primary,
    /// `Continue` - the blue confirm.
    Confirm,
}

/// `enabled` reflects `!op_in_flight` at every real call site (see `Self::
/// render_rebase_banner_actions`'s own docs) - a disabled button renders dimmed, with no
/// `cursor_pointer`, and (the caller's own job - see that method) no `.on_click` attached at all,
/// so it is genuinely inert, not just styled to look that way.
fn render_rebase_button(
    label: &'static str,
    style: RebaseButtonStyle,
    enabled: bool,
    keycaps: &[String],
) -> gpui::Stateful<gpui::Div> {
    let mut button = div()
        .id(format!("rebase-banner-button-{label}"))
        .debug_selector(move || format!("rebase-banner-button-{label}"))
        .h(px(23.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .rounded(theme::radius::BUTTON)
        .when(keycaps.is_empty(), |el| el.px(px(10.0)))
        // §1.2's own asymmetric padding on a keycapped button: `0 4px 0 10px`, so the caps sit
        // closer to the right edge than the label does to the left.
        .when(!keycaps.is_empty(), |el| el.pl(px(10.0)).pr(px(4.0)))
        .when(enabled, |el| el.cursor_pointer())
        .when(!enabled, |el| el.cursor_default().opacity(0.45))
        .font(font(theme::font::SANS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(10.5));
    button = match style {
        RebaseButtonStyle::Primary => button
            .bg(theme::button::GREEN_BG)
            .border_1()
            .border_color(theme::button::GREEN_KEYCAP)
            .text_color(theme::button::GREEN_FG)
            .when(enabled, |el| {
                el.hover(|el| el.bg(theme::button::GREEN_BG_HOVER))
            }),
        RebaseButtonStyle::Confirm => button
            .bg(theme::button::BLUE_BG)
            .border_1()
            .border_color(theme::button::BLUE_KEYCAP)
            .text_color(theme::button::BLUE_FG)
            .when(enabled, |el| {
                el.hover(|el| el.bg(theme::button::BLUE_BG_HOVER))
            }),
        RebaseButtonStyle::Ghost | RebaseButtonStyle::GhostDanger => button
            .text_color(if style == RebaseButtonStyle::GhostDanger {
                theme::button::DANGER_FG
            } else {
                theme::text::DIM
            })
            .when(enabled, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            }),
    };
    button = button.child(label);
    if !keycaps.is_empty() {
        let (fg, border) = match style {
            RebaseButtonStyle::Primary => (
                theme::button::GREEN_KEYCAP_FG.resolve(),
                theme::button::GREEN_KEYCAP.resolve(),
            ),
            _ => (
                theme::button::BLUE_FG.resolve(),
                theme::button::BLUE_KEYCAP.resolve(),
            ),
        };
        button = button.child(crate::root::widgets::render_action_keycap_row(
            keycaps, fg, border,
        ));
    }
    button
}

/// Design spec §1.4's **fold elbow** (GitHub issue #303), verbatim: "20 wide on `squash`/`fixup`
/// rows only: a 1px elbow in the action's colour - inset 5 each side, `top:-1` so it meets the row
/// above's edge, `bottom:13` so it lands on the chip centreline, `border-left` + `border-bottom`,
/// 5px corner radius. Reads up-and-left into the commit being folded into. **Same vocabulary as
/// the graph's merge elbows**."
fn render_rebase_fold_elbow(
    index: usize,
    folded: bool,
    action_fg: theme::ColorToken,
) -> gpui::AnyElement {
    let mut slot = div()
        .flex_none()
        .w(if folded {
            theme::graph::REBASE_FOLD_INDENT
        } else {
            px(0.0)
        })
        .self_stretch()
        .relative();
    if folded {
        slot = slot.child(
            div()
                .debug_selector(move || format!("rebase-plan-row-{index}-fold-elbow"))
                .absolute()
                .left(theme::graph::REBASE_FOLD_ELBOW_INSET_X)
                .right(theme::graph::REBASE_FOLD_ELBOW_INSET_X)
                .top(theme::graph::REBASE_FOLD_ELBOW_TOP)
                .bottom(theme::graph::REBASE_FOLD_ELBOW_BOTTOM)
                .border_l(theme::graph::LINE_WIDTH)
                .border_b(theme::graph::LINE_WIDTH)
                .border_color(action_fg)
                .rounded_bl(theme::graph::ELBOW_RADIUS),
        );
    }
    slot.into_any_element()
}

/// Design spec §1.3's column header, 22 high: `action` 104 (13 left pad, clearing the rows' 2px
/// selection edge) · `commit` flex · `files` 62 right · `sha` 62 right · **pause column 22,
/// carrying an outlined 5px square** as its label.
fn render_rebase_column_header() -> gpui::AnyElement {
    // The exact same label treatment `render_graph_header`'s own inner `label` helper uses
    // (§1.3: "Same treatment as the graph's header") - 9px/450 Plex Sans, uppercased in code
    // rather than authored uppercase, so the two headers can only ever look the same.
    fn label(text: &'static str) -> impl IntoElement {
        div()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight(450.0))
            .text_size(px(9.0))
            .text_color(theme::graph::HEADER_LABEL_FG)
            .child(text.to_uppercase())
    }

    div()
        .id("rebase-column-header")
        .debug_selector(|| "rebase-column-header".to_string())
        .flex_none()
        .flex()
        .items_center()
        .w_full()
        .h(theme::graph::HEADER)
        .bg(theme::graph::HEADER_BG)
        .border_b_1()
        .border_color(theme::border::INNER)
        .child(
            div()
                .flex_none()
                .w(theme::graph::REBASE_COL_ACTION)
                .pl(theme::graph::REBASE_COL_ACTION_PAD)
                .child(label("action")),
        )
        .child(div().flex_1().min_w_0().child(label("commit")))
        .child(
            div()
                .flex_none()
                .w(theme::graph::REBASE_COL_NUMERIC)
                .text_right()
                .child(label("files")),
        )
        .child(
            div()
                .flex_none()
                .w(theme::graph::REBASE_COL_NUMERIC)
                .text_right()
                .child(label("sha")),
        )
        .child(
            div()
                .id("rebase-column-header-pause")
                .debug_selector(|| "rebase-column-header-pause".to_string())
                .flex_none()
                .w(theme::graph::REBASE_COL_PAUSE)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .debug_selector(|| "rebase-column-header-pause-mark".to_string())
                        .w(theme::graph::REBASE_MARK)
                        .h(theme::graph::REBASE_MARK)
                        .rounded(theme::radius::MARK_SM)
                        .border_1()
                        .border_color(theme::graph::REBASE_HEADER_PAUSE_MARK),
                ),
        )
        .into_any_element()
}

/// Design spec §1.5: an **outlined** 5px square for a planned pause, a **filled** one for where
/// the rebase actually stopped. `edit`'s and a message-less `reword`'s planned/actual markers
/// render identically - the design spec itself says the visual distinction between the two
/// `StopReason`s isn't load-bearing.
fn render_rebase_pause_marker(planned: bool, actual: bool) -> gpui::AnyElement {
    if !planned && !actual {
        return gpui::Empty.into_any_element();
    }
    div()
        .flex_none()
        .w(theme::graph::REBASE_MARK)
        .h(theme::graph::REBASE_MARK)
        .rounded(theme::radius::MARK_SM)
        .when(actual, |el| el.bg(theme::status::ASK))
        // §1.5's own two colours: the filled mark is the attention amber `#e2a336`, the outlined
        // one the dimmer `#8a6420` edge - a planned pause is not yet an event.
        .when(!actual, |el| {
            el.border_1().border_color(theme::status::ASK_CARD_EDGE)
        })
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

        let row = cx
            .debug_bounds("graph-row-2")
            .expect("row 2 must be painted");
        right_click(cx, row.center());

        let option = cx
            .debug_bounds("dropdown-menu-row-Rebase onto this commit")
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

    // --- The derived Result list is checked against what real git actually produces -------------

    #[gpui::test]
    fn the_derived_result_blocks_match_the_real_history_a_real_rebase_produces(
        cx: &mut TestAppContext,
    ) {
        // Independent files throughout: this test is comparing a derivation against real git
        // output, so a conflict getting in the way would only obscure what it is measuring.
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        commit(repo.path(), "base.txt", "base", "base");
        commit(repo.path(), "a.txt", "1", "one");
        commit(repo.path(), "b.txt", "1", "two");
        commit(repo.path(), "c.txt", "1", "three");
        commit(repo.path(), "d.txt", "1", "four");
        commit(repo.path(), "e.txt", "1", "five");
        commit(repo.path(), "f.txt", "1", "six");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(6, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let plan = &app
                .graph_state
                .rebase
                .as_ref()
                .expect("in rebase mode")
                .plan;
            let subjects: Vec<&str> = plan
                .iter()
                .map(|row| row.original_subject.as_str())
                .collect();
            assert_eq!(
                subjects,
                vec!["one", "two", "three", "four", "five", "six"],
                "the plan must be oldest-first, excluding the onto row (`base`) itself"
            );
        });

        for (index, action) in [
            (0, super::rebase::RebaseActionKind::Pick),
            (1, super::rebase::RebaseActionKind::Squash),
            (2, super::rebase::RebaseActionKind::Fixup),
            (3, super::rebase::RebaseActionKind::Reword),
            (4, super::rebase::RebaseActionKind::Drop),
            (5, super::rebase::RebaseActionKind::Pick),
        ] {
            app.update_in(cx, |app, _window, cx| {
                app.set_rebase_row_action(index, action, cx);
            });
        }
        cx.run_until_parked();

        // The reword message is supplied through the real, focused field with real keystrokes -
        // the same path a user takes, not fabricated state.
        let focus_handle = app.read_with(cx, |app, _| {
            app.graph_state
                .rebase
                .as_ref()
                .expect("in rebase mode")
                .plan[3]
                .reword_focus_handle
                .clone()
        });
        app.update_in(cx, |_app, window, cx| {
            window.focus(&focus_handle, cx);
        });
        cx.simulate_input(" reworded");
        cx.run_until_parked();

        let derived = app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            let blocks = super::rebase::derive_result_blocks(&rebase_state.plan);
            assert_eq!(
                super::rebase::derive_result_commit_count(&rebase_state.plan),
                blocks.len(),
                "the banner's own N -> M number must be exactly the Result list's length"
            );
            assert_eq!(rebase_state.plan.len(), 6);
            blocks
                .iter()
                .map(|block| (block.subject.clone(), block.folded_count, block.status))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            derived,
            vec![
                (
                    "one".to_string(),
                    2,
                    super::rebase::ResultBlockStatus::Normal
                ),
                (
                    "four reworded".to_string(),
                    0,
                    super::rebase::ResultBlockStatus::Reworded
                ),
                (
                    "six".to_string(),
                    0,
                    super::rebase::ResultBlockStatus::Normal
                ),
            ],
            "the derivation must fold `two`/`three` into `one`, skip the dropped `five`, and \
             carry the live reworded text"
        );

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "a plan with no `edit` row and a message-supplied `reword` must run straight \
                 through to a real Completed outcome"
            );
        });

        // The real, on-disk history the real rebase produced - the ground truth the derivation
        // above is being judged against.
        let real_log = git_output(repo.path(), &["log", "--format=%s", "--reverse"]);
        let real_subjects: Vec<&str> = real_log.lines().collect();
        assert_eq!(
            real_subjects,
            vec!["base", "one", "four reworded", "six"],
            "the real rebase must genuinely produce the history the Result panel predicted"
        );
        let derived_subjects: Vec<&str> = derived
            .iter()
            .map(|(subject, _, _)| subject.as_str())
            .collect();
        assert_eq!(
            real_subjects[1..],
            derived_subjects[..],
            "every derived Result block must correspond, in order, to a real resulting commit - \
             the Result list is a prediction of real git output, not an authored one"
        );

        // The derived `folded_count: 2` must be real too: `squash` keeps the folded commit's own
        // message, so `two` must genuinely be in the resulting commit's body.
        let folded_body = git_output(repo.path(), &["log", "-1", "--format=%B", "HEAD~2"]);
        assert!(
            folded_body.contains("two"),
            "the squashed commit's message must really carry the folded commit's own message, \
             got {folded_body:?}"
        );
        assert!(
            !folded_body.contains("three"),
            "`fixup` must really discard its own message, unlike `squash` - got {folded_body:?}"
        );
    }

    // --- A reword message supplied up front removes the pause for real --------------------------

    #[gpui::test]
    fn a_reword_message_supplied_before_start_runs_straight_through_with_no_stop(
        cx: &mut TestAppContext,
    ) {
        let (repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(1, super::rebase::RebaseActionKind::Reword, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            assert!(
                rebase_state.plan[1].is_planned_pause(),
                "before a message is supplied, the reword row is a planned pause - the mark"
            );
            assert_eq!(
                super::rebase::derive_stop_count(&rebase_state.plan),
                1,
                "...and it counts toward `Stops N times` - the count"
            );
        });

        let focus_handle = app.read_with(cx, |app, _| {
            app.graph_state
                .rebase
                .as_ref()
                .expect("in rebase mode")
                .plan[1]
                .reword_focus_handle
                .clone()
        });
        app.update_in(cx, |_app, window, cx| {
            window.focus(&focus_handle, cx);
        });
        cx.simulate_input(" supplied up front");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            assert!(
                !rebase_state.plan[1].is_planned_pause(),
                "supplying the message must remove the row's own planned-pause mark"
            );
            assert_eq!(
                super::rebase::derive_stop_count(&rebase_state.plan),
                0,
                "...and remove it from the `Stops N times` count, which at zero renders nothing"
            );
            assert_eq!(
                rebase_state.plan[1].to_plan_entry().action,
                wt_core::rebase::RebaseAction::Reword(Some("third supplied up front".to_string())),
                "the real plan entry handed to wt_core must carry the supplied message, so the \
                 real rebase never has to stop for it"
            );
        });

        app.update_in(cx, |app, _window, cx| {
            app.start_rebase(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.graph_state.rebase.is_none(),
                "a reword whose message was supplied before Start must never hand control back - \
                 the real rebase runs straight through to Completed"
            );
        });
        let subjects = git_output(repo.path(), &["log", "--format=%s", "--reverse"]);
        assert_eq!(
            subjects, "base\nsecond\nthird supplied up front",
            "the real history must carry the message supplied up front, with no stop in between"
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

    #[gpui::test]
    fn closing_the_graph_tab_mid_rebase_resumes_a_paused_agent(cx: &mut TestAppContext) {
        let (repo, app, cx) = open_seeded_graph(cx);

        let agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
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

    // --- Design spec §1.3/§1.4/§1.5's real geometry (GitHub issues #303, #304) ---------------

    #[gpui::test]
    fn a_fold_row_indents_its_chip_by_twenty_and_paints_a_real_elbow(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        let picked = cx
            .debug_bounds("rebase-action-chip-1")
            .expect("row 1's chip must be painted while it is still a `pick`");
        assert!(
            cx.debug_bounds("rebase-plan-row-1-fold-elbow").is_none(),
            "a `pick` row must paint no fold elbow at all - the elbow is the mark of a fold, and \
             a slot every row carries could never make a fold row read as indented"
        );

        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(1, super::rebase::RebaseActionKind::Squash, cx);
        });
        cx.run_until_parked();

        let folded = cx
            .debug_bounds("rebase-action-chip-1")
            .expect("row 1's chip must still be painted once it folds");
        assert_eq!(
            f32::from(folded.origin.x - picked.origin.x),
            20.0,
            "design spec §1.4: a fold row indents 20 (chip at {:?} while picked, {:?} while \
             squashed)",
            picked.origin,
            folded.origin
        );

        let elbow = cx
            .debug_bounds("rebase-plan-row-1-fold-elbow")
            .expect("a folding row must paint a real elbow element");
        let row = cx
            .debug_bounds("rebase-plan-row-1")
            .expect("row 1 must be painted");
        assert_eq!(
            f32::from(elbow.size.width),
            10.0,
            "§1.4's elbow is inset 5 each side of its 20-wide slot"
        );
        assert_eq!(
            f32::from(elbow.origin.y - row.origin.y),
            -1.0,
            "§1.4's `top:-1` is what makes the elbow meet the row above's edge"
        );
        assert_eq!(
            f32::from(elbow.bottom_left().y - row.origin.y),
            15.0,
            "§1.4's `bottom:13` on a 28-high row puts the elbow's own inside-painted bottom \
             border just above 15 - the centreline of an 18-high chip centred in 28"
        );
        assert_eq!(
            f32::from(folded.center().y - row.origin.y),
            14.0,
            "...which is exactly where the chip's centreline really is"
        );
        assert!(
            elbow.origin.x < folded.origin.x,
            "the elbow reads up-and-*left* into the commit being folded into, so it sits left \
             of the chip it belongs to ({elbow:?} vs {folded:?})"
        );
    }

    #[gpui::test]
    fn the_order_note_lives_in_a_pinned_footer_below_the_scrolling_plan_list(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        git(repo.path(), &["config", "user.email", "test@example.com"]);
        git(repo.path(), &["config", "user.name", "Test User"]);
        for n in 0..40 {
            commit(
                repo.path(),
                &format!("f{n}.txt"),
                "x",
                &format!("commit {n}"),
            );
        }

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_git_graph(window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(39, cx);
        });
        cx.run_until_parked();

        let rows = cx
            .debug_bounds("rebase-plan-rows")
            .expect("the scrolling plan list must be painted");
        let footer = cx
            .debug_bounds("rebase-plan-footer")
            .expect("design spec §1.4's footer band must exist");
        let note = cx
            .debug_bounds("rebase-order-note")
            .expect("the order note must be painted");
        let legend = cx
            .debug_bounds("rebase-pause-legend")
            .expect("§1.5's `pauses here` legend must be painted, once, in the footer");

        assert_eq!(
            f32::from(footer.size.height),
            28.0,
            "§1.4: the footer is 28 high"
        );
        assert!(
            footer.origin.y >= rows.bottom_left().y,
            "the footer must sit below the scroller, not inside it (rows {rows:?}, footer \
             {footer:?})"
        );
        for (name, bounds) in [("order note", note), ("pauses-here legend", legend)] {
            assert!(
                footer.contains(&bounds.origin) && footer.contains(&bounds.bottom_right()),
                "the {name} must be painted inside the pinned footer band, so a long plan can \
                 never scroll it away (footer {footer:?}, {name} {bounds:?})"
            );
        }
    }

    #[gpui::test]
    fn the_pause_column_is_legended_in_the_header_and_once_more_in_the_footer(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        let header_mark = cx
            .debug_bounds("rebase-column-header-pause-mark")
            .expect("§1.3: the pause column header carries an outlined 5px square as its label");
        assert_eq!(f32::from(header_mark.size.width), 5.0);
        assert_eq!(f32::from(header_mark.size.height), 5.0);

        let header_cell = cx
            .debug_bounds("rebase-column-header-pause")
            .expect("the header's pause cell must be painted");
        assert_eq!(
            f32::from(header_cell.size.width),
            22.0,
            "§1.3: pause column 22 wide"
        );

        // ...and the same column in a row lines up under it, which is the whole point of the
        // header carrying the mark rather than a word.
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Edit, cx);
        });
        cx.run_until_parked();
        let row_mark = cx
            .debug_bounds("rebase-plan-row-0")
            .expect("row 0 must be painted");
        let probe = gpui::Point {
            x: header_cell.center().x,
            y: row_mark.center().y,
        };
        assert!(
            row_mark.contains(&probe),
            "the pause column's header cell must sit over the rows it labels (header cell \
             {header_cell:?}, row {row_mark:?})"
        );
    }

    // --- The action menu (GitHub issue #302) ---------------------------------------------------

    #[gpui::test]
    fn the_action_menu_is_274_wide_and_paints_all_six_actions(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        let chip = cx
            .debug_bounds("rebase-action-chip-0")
            .expect("row 0's chip must be painted");
        cx.simulate_click(chip.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        // `debug_bounds` takes a `&'static str`, so the six selectors are literals - listed in
        // `RebaseActionKind::ALL`'s own order and length-checked against it, so an action added
        // to that list without a selector here fails rather than being silently skipped.
        const OPTION_SELECTORS: [&str; 6] = [
            "rebase-action-option-0-pick",
            "rebase-action-option-0-reword",
            "rebase-action-option-0-edit",
            "rebase-action-option-0-squash",
            "rebase-action-option-0-fixup",
            "rebase-action-option-0-drop",
        ];
        assert_eq!(
            OPTION_SELECTORS.len(),
            super::rebase::RebaseActionKind::ALL.len()
        );
        let menu = cx
            .debug_bounds("rebase-action-menu-0")
            .expect("the open action menu must be painted");
        assert_eq!(
            f32::from(menu.size.width),
            274.0,
            "§1.4: the action menu is 274 wide - it used to be 90, which could not fit a hint \
             at all"
        );

        let mut option_bounds = Vec::new();
        for selector in OPTION_SELECTORS {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} must be painted"));
            // Every option row spans the menu's own inner width - the popover's 1px border on
            // each side, and nothing else, separates it from the menu's own 274.
            assert_eq!(
                f32::from(bounds.size.width),
                272.0,
                "{selector}'s row must span the whole 274-wide menu less its 1px borders"
            );
            option_bounds.push(bounds);
        }
        let row = cx
            .debug_bounds("rebase-plan-row-0")
            .expect("row 0 must be painted");
        assert!(
            option_bounds[0].origin.y >= row.bottom_left().y,
            "the action menu must never overlap the row it belongs to (row {row:?}, first \
             option {:?})",
            option_bounds[0]
        );
    }

    // --- The real keyboard bindings behind §1.4's footer hints (GitHub issue #304) -------------

    #[gpui::test]
    fn pressing_the_footers_own_letter_really_changes_the_selected_rows_action(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();

        let row = cx
            .debug_bounds("rebase-plan-row-1")
            .expect("row 1 must be painted");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .rebase
                    .as_ref()
                    .expect("in rebase mode")
                    .selected_index(),
                Some(1),
                "premise: a real click really selects the row it landed on"
            );
        });

        cx.simulate_keystrokes("s");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .rebase
                    .as_ref()
                    .expect("in rebase mode")
                    .plan[1]
                    .action,
                super::rebase::RebaseActionKind::Squash,
                "a real `S` keystroke must squash the selected row"
            );
        });

        cx.simulate_keystrokes("d");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .rebase
                    .as_ref()
                    .expect("in rebase mode")
                    .plan[1]
                    .action,
                super::rebase::RebaseActionKind::Drop,
            );
        });

        cx.simulate_keystrokes("p");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.graph_state
                    .rebase
                    .as_ref()
                    .expect("in rebase mode")
                    .plan[1]
                    .action,
                super::rebase::RebaseActionKind::Pick,
            );
        });
    }

    #[gpui::test]
    fn alt_arrow_really_reorders_the_selected_row_and_the_selection_follows_it(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let plan = &app
                .graph_state
                .rebase
                .as_ref()
                .expect("in rebase mode")
                .plan;
            assert_eq!(
                plan.iter()
                    .map(|row| row.original_subject.as_str())
                    .collect::<Vec<_>>(),
                vec!["second", "third"],
                "premise: the plan starts oldest-first"
            );
        });

        let row = cx
            .debug_bounds("rebase-plan-row-1")
            .expect("row 1 must be painted");
        cx.simulate_click(row.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        cx.simulate_keystrokes("alt-up");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            assert_eq!(
                rebase_state
                    .plan
                    .iter()
                    .map(|row| row.original_subject.as_str())
                    .collect::<Vec<_>>(),
                vec!["third", "second"],
                "a real alt+↑ must really move the selected row up the plan"
            );
            assert_eq!(
                rebase_state.selected_index(),
                Some(0),
                "the selection must follow the row it moved, not stay at the old index"
            );
        });
    }

    #[gpui::test]
    fn typing_into_a_reword_field_never_fires_the_plans_own_letter_shortcuts(
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
        cx.run_until_parked();

        cx.simulate_keystrokes("s");
        cx.simulate_keystrokes("p");
        cx.simulate_keystrokes("d");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let rebase_state = app.graph_state.rebase.as_ref().expect("in rebase mode");
            assert_eq!(
                rebase_state.plan[0].action,
                super::rebase::RebaseActionKind::Reword,
                "the focused reword field must keep every letter to itself - a `p` here is a \
                 character in a commit message, not the `pick` shortcut"
            );
            assert!(
                rebase_state.plan[0]
                    .reword_message
                    .as_str()
                    .ends_with("spd"),
                "...and those three letters must really have landed in the message, got {:?}",
                rebase_state.plan[0].reword_message.as_str()
            );
        });
    }

    // --- The warning stack and the stopped strip (GitHub issue #305) ---------------------------

    #[gpui::test]
    fn each_warning_row_paints_a_real_dot_title_and_body(cx: &mut TestAppContext) {
        let (_repo, app, cx) = open_seeded_graph(cx);
        app.update_in(cx, |app, _window, cx| {
            app.enter_rebase_mode(2, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, _window, cx| {
            app.set_rebase_row_action(0, super::rebase::RebaseActionKind::Edit, cx);
        });
        cx.run_until_parked();

        let dot = cx
            .debug_bounds("rebase-warning-stops-dot")
            .expect("§1.7: every warning row carries a 5px severity square");
        let title = cx
            .debug_bounds("rebase-warning-stops-title")
            .expect("§1.7: title and body are two real lines, not one concatenated sentence");
        let body = cx
            .debug_bounds("rebase-warning-stops-body")
            .expect("§1.7: ...and the body is the second of them");

        assert_eq!(f32::from(dot.size.width), 5.0);
        assert_eq!(f32::from(dot.size.height), 5.0);
        assert!(
            dot.origin.x < title.origin.x,
            "the square leads the row (dot {dot:?}, title {title:?})"
        );
        assert!(
            body.origin.y > title.origin.y,
            "the body sits under its own title (title {title:?}, body {body:?})"
        );
    }

    #[gpui::test]
    fn the_stopped_strip_paints_the_amber_square_the_spec_asks_for(cx: &mut TestAppContext) {
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

        let strip = cx
            .debug_bounds("rebase-stopped-strip")
            .expect("a real conflict stop must paint the strip");
        let mark = cx
            .debug_bounds("rebase-stopped-strip-mark")
            .expect("§1.8's own 5px square - the strip used to have no mark at all");
        let text = cx
            .debug_bounds("rebase-stopped-strip-text")
            .expect("the progress text must be painted");
        assert_eq!(f32::from(mark.size.width), 5.0);
        assert_eq!(f32::from(mark.size.height), 5.0);
        assert!(strip.contains(&mark.origin));
        assert!(
            mark.origin.x < text.origin.x,
            "the square leads the strip, exactly like a warning row's own"
        );
    }

    #[cfg(not(target_os = "linux"))]
    fn wait_for_proc_state(_pid: u32, _prefix: &str) {}
    #[cfg(not(target_os = "linux"))]
    fn wait_for_proc_state_not(_pid: u32, _prefix: &str) {}
}

/// The design spec's own tables, asserted as tables (GitHub issues #302 and #305).
#[cfg(test)]
mod rebase_design_tests {
    use super::rebase::RebaseActionKind;
    use super::{rebase_action_style, RebaseWarningKind, REBASE_FOOTER_HINTS};
    use crate::theme::hex_rgba;

    /// `revision 5/REVISION-2026-08-12.md` §1.4's action table, transcribed here as data so a
    /// drift in either direction is a failing assertion rather than a review catch.
    fn spec_action_table() -> Vec<(RebaseActionKind, u32, u32, u32)> {
        vec![
            (RebaseActionKind::Pick, 0xa9b0b7, 0x1c2023, 0x2a2f34),
            (RebaseActionKind::Reword, 0x8fbde6, 0x1d2532, 0x2b3d4f),
            (RebaseActionKind::Edit, 0xd8a94a, 0x2b2413, 0x3f3418),
            (RebaseActionKind::Squash, 0x7fc79a, 0x16261e, 0x24503a),
            (RebaseActionKind::Fixup, 0x5f9c78, 0x16261e, 0x1e3b2a),
            (RebaseActionKind::Drop, 0xc4726d, 0x2a1719, 0x4a2422),
        ]
    }

    #[test]
    fn every_action_chip_wears_the_design_specs_own_three_colours() {
        for (action, fg, bg, border) in spec_action_table() {
            let style = rebase_action_style(action);
            assert_eq!(
                style.fg.default,
                hex_rgba(fg),
                "{}'s chip foreground",
                action.label()
            );
            assert_eq!(
                style.bg.default,
                hex_rgba(bg),
                "{}'s chip background",
                action.label()
            );
            assert_eq!(
                style.border.default,
                hex_rgba(border),
                "{}'s chip border",
                action.label()
            );
        }
    }

    #[test]
    fn no_two_actions_are_the_same_colour_and_the_two_folding_verbs_share_a_background() {
        let mut seen: Vec<gpui::Rgba> = Vec::new();
        for action in RebaseActionKind::ALL {
            let fg = rebase_action_style(action).fg.default;
            assert!(
                !seen.contains(&fg),
                "{} reuses another action's foreground - the chips would carry no information",
                action.label()
            );
            seen.push(fg);
        }
        let squash = rebase_action_style(RebaseActionKind::Squash);
        let fixup = rebase_action_style(RebaseActionKind::Fixup);
        assert_eq!(
            squash.bg.default, fixup.bg.default,
            "squash and fixup share a hue on purpose - they do the same thing to history"
        );
        assert_ne!(
            squash.fg.default, fixup.fg.default,
            "...but squash is the brighter of the pair, so they stay distinguishable"
        );
    }

    #[test]
    fn every_action_has_the_design_specs_own_one_line_menu_hint() {
        let expected = [
            (RebaseActionKind::Pick, "keep the commit as it is"),
            (RebaseActionKind::Reword, "stop to edit the message"),
            (RebaseActionKind::Edit, "stop to amend the contents"),
            (RebaseActionKind::Squash, "fold up, keep both messages"),
            (RebaseActionKind::Fixup, "fold up, discard this message"),
            (RebaseActionKind::Drop, "remove the commit"),
        ];
        assert_eq!(expected.len(), RebaseActionKind::ALL.len());
        for (action, hint) in expected {
            assert_eq!(action.hint(), hint, "{}'s menu hint", action.label());
            assert!(
                !action.hint().contains('\n'),
                "{}'s hint must be one line",
                action.label()
            );
        }
    }

    #[test]
    fn the_three_warning_severities_are_three_genuinely_different_colours() {
        let expected = [
            (RebaseWarningKind::RunningAgents, 0xe2a336),
            (RebaseWarningKind::RemoteCommits, 0x8fbde6),
            (RebaseWarningKind::StopCount, 0x565d64),
        ];
        let mut seen: Vec<gpui::Rgba> = Vec::new();
        for (kind, hex) in expected {
            let dot = kind.dot().default;
            assert_eq!(dot, hex_rgba(hex), "{kind:?}'s severity dot");
            assert!(
                !seen.contains(&dot),
                "{kind:?} reuses another warning's severity colour - the gradient is flat again"
            );
            seen.push(dot);
        }
    }

    #[test]
    fn every_footer_keycap_hint_is_backed_by_a_really_registered_binding() {
        // Each footer hint, paired with the real action name(s) and registered keystroke(s) it
        // advertises. `alt+↑↓` is one hint over two real bindings, which is why this is a list
        // per hint rather than one entry each.
        let expected: Vec<(&str, Vec<(&str, &str)>)> = vec![
            (
                "alt+\u{2191}\u{2193}",
                vec![
                    ("app::RebaseReorderUp", "alt-up"),
                    ("app::RebaseReorderDown", "alt-down"),
                ],
            ),
            ("P", vec![("app::RebasePickRow", "p")]),
            ("S", vec![("app::RebaseSquashRow", "s")]),
            ("D", vec![("app::RebaseDropRow", "d")]),
            (
                "mod+enter",
                vec![(
                    "app::RebaseStart",
                    if cfg!(target_os = "macos") {
                        "cmd-enter"
                    } else {
                        "ctrl-enter"
                    },
                )],
            ),
        ];
        assert_eq!(
            expected.len(),
            REBASE_FOOTER_HINTS.len(),
            "a footer hint was added or removed without saying which real binding it names"
        );

        let bindings = crate::default_key_bindings();
        for ((spec, _label), (expected_spec, actions)) in
            REBASE_FOOTER_HINTS.iter().zip(expected.iter())
        {
            assert_eq!(spec, expected_spec, "footer hints are asserted in order");
            for (action, keystroke) in actions {
                let matching: Vec<&gpui::KeyBinding> = bindings
                    .iter()
                    .filter(|binding| {
                        binding.action().name() == *action
                            && binding.keystrokes().len() == 1
                            && binding.keystrokes()[0].inner().unparse() == *keystroke
                    })
                    .collect();
                assert_eq!(
                    matching.len(),
                    1,
                    "the footer paints a {spec:?} keycap, so exactly one real {action} binding \
                     on {keystroke:?} must exist behind it"
                );
                // ...and it must be scoped to this surface, never global: a bare `p` claimed
                // app-wide would swallow the letter everywhere in the app.
                assert!(
                    matching[0].predicate().is_some(),
                    "{action} must be scoped to the rebase plan, never registered globally"
                );
            }
        }
    }
}
