//! The real sidebar History body (GitHub issue #227): the scope toggle, the count line, the
//! repo → worktree → run tree, and the click that opens a run's transcript.
//!
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-13.md` §3 is the whole brief for this
//! file, in one sentence: History "is a **list you pick from**, like a file tree - not a
//! document. Centre tabs hold things you read or work in; the sidebar holds things you navigate."
//! So nothing here renders a transcript; a row's only job is to open one
//! ([`crate::run_history::tab`]).
//!
//! `REVISION-2026-08-14.md` §6 amends §3's flat, one-worktree list into the hierarchy this file
//! paints: "**Agent history** is repo → worktree → run, matching the rail, with an `all` /
//! `this worktree` scope toggle. Active worktree carries the blue edge and opens by default."
//!
//! Every decision about *what a row says* lives in [`crate::run_history::model`] and is asserted
//! there without a window; this file is the `gpui::Div` half only, exactly the split
//! [`crate::rail::strip`]/[`crate::rail::strip_render`] already keep for the Problems view one
//! module over.

use super::*;

use crate::rail::strip::SidebarView;
use crate::root::widgets::{render_disclosure_caret, render_sidebar_message, text_tooltip};
use crate::run_history::model::{self, HistoryScope, RunEntry, RunGroup, RunTree};

impl AdeApp {
    /// Switches the sidebar to History - the `⋯` overflow's own `History` row
    /// (`crate::rail::menu::RailMenuAction::OpenHistory`).
    ///
    /// Kicks the real drift traversal off here as well as on the body's own render, so the very
    /// first History frame already has an answer to paint for a window that has been open a
    /// while: `load_run_drift` is single-flight and skips every worktree it has already answered,
    /// so calling it from both places costs nothing.
    pub(crate) fn open_history_view(&mut self, cx: &mut Context<Self>) {
        self.set_sidebar_view(SidebarView::History, cx);
        self.load_run_drift(cx);
    }

    /// A worktree row's own `↺ N earlier runs` line (`REVISION-2026-08-13.md` §6): selects that
    /// checkout, switches the sidebar to History and narrows the scope to it.
    ///
    /// Narrowing the scope is what makes this line land somewhere - §6 says it switches "the
    /// sidebar to History **for that worktree**", and the `all` scope's whole-window list would
    /// answer a different question than the row that was clicked. The toggle is right there to
    /// widen it back, so this is a starting position rather than a mode.
    pub(crate) fn open_history_for_worktree(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_worktree_by_path(&path, window, cx);
        self.history_scope = HistoryScope::ThisWorktree;
        // An explicit earlier fold of this group would otherwise hide the very runs this click
        // asked to see.
        self.history_collapsed.remove(&path);
        self.open_history_view(cx);
    }

    /// The `all` / `this worktree` toggle's own click.
    pub(crate) fn set_history_scope(&mut self, scope: HistoryScope, cx: &mut Context<Self>) {
        if self.history_scope == scope {
            return;
        }
        self.history_scope = scope;
        cx.notify();
    }

    /// Folds or unfolds one worktree group, recording the choice explicitly - see
    /// [`Self::history_collapsed`] on why this is a map of decisions rather than a set of open
    /// groups.
    fn toggle_history_group(&mut self, path: PathBuf, was_open: bool, cx: &mut Context<Self>) {
        self.history_collapsed.insert(path, was_open);
        cx.notify();
    }

    /// The real repo → worktree → run tree for whatever the window knows right now, already
    /// scoped, filtered and folded.
    ///
    /// One place builds it, so the count line, the rows and the two empty notes are three
    /// readings of one pass - the same guarantee `crate::rail::render::AdeApp::render_rail` gives
    /// the strip's marker and the Problems body (§2: "tallied over their own data").
    pub(crate) fn history_run_tree(&self) -> RunTree {
        model::build_run_tree(
            &self.past_runs(),
            &self.history_worktrees(),
            self.current_worktree_path().as_deref(),
            self.history_scope,
            self.filter_query.as_str(),
            &self.history_collapsed,
            &self.run_drift,
        )
    }

    /// The whole History view.
    pub(crate) fn render_history_view(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // The one real background load this surface needs, asked for from the body that needs it
        // - see `crate::run_history::flow::AdeApp::load_run_drift`.
        self.load_run_drift(cx);

        let tree = self.history_run_tree();
        let branch = self.current_worktree_branch_label();

        let mut body = div()
            .id("sidebar-history")
            .debug_selector(|| "sidebar-history".to_string())
            .flex()
            .flex_col()
            .child(self.render_history_scope_toggle(branch.as_deref(), cx));

        if tree.is_empty() {
            // Two different facts, said as two (§4g, and the Problems view's own pair one module
            // over): a window with no history at all, and a filter that hid all of it.
            let note = if tree.unfiltered == 0 {
                model::empty_note(self.history_scope, branch.as_deref())
            } else {
                model::filtered_away_note(tree.unfiltered)
            };
            return body
                .child(
                    div()
                        .debug_selector(|| "sidebar-history-note".to_string())
                        .child(render_sidebar_message(note, theme::text::GHOST.into())),
                )
                .into_any_element();
        }

        if let Some(line) = tree.count_line() {
            body = body.child(
                div()
                    .flex_none()
                    .px(px(12.0))
                    .pt(px(8.0))
                    .pb(px(6.0))
                    .debug_selector(|| "sidebar-history-count".to_string())
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::FAINTER)
                    .child(line),
            );
        }

        for repo in &tree.repos {
            // A repo header only earns its line when there is more than one repo to tell apart -
            // §4's own rule for the rail's repo headers, and the reason a single-repo window does
            // not pay a row to be told the name it already has in the title bar.
            if tree.repos.len() > 1 {
                body = body.child(
                    div()
                        .flex_none()
                        .px(px(12.0))
                        .pt(px(7.0))
                        .pb(px(3.0))
                        .font(font(theme::font::MONO))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::history::REPO_LABEL)
                        .child(repo.label.to_uppercase()),
                );
            }
            for group in &repo.groups {
                body = body.child(self.render_history_group(group, cx));
                if group.open {
                    for entry in &group.runs {
                        body = body.child(self.render_history_run_row(entry, cx));
                    }
                }
            }
        }

        body.into_any_element()
    }

    /// The `all` / `this worktree` toggle (`REVISION-2026-08-14.md` §6).
    ///
    /// Two segments, drawn from one `map` over [`HistoryScope::ALL`] rather than two hand-written
    /// pills, for §7 rule 7's reason - a control drawn twice is one control, and two copies drift
    /// on padding first. Each carries its own tooltip, since `this worktree` is only meaningful
    /// once you know which checkout that is.
    fn render_history_scope_toggle(
        &self,
        branch: Option<&str>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = self.history_scope;
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(4.0))
            .px(px(12.0))
            .h(px(27.0))
            .debug_selector(|| "history-scope-toggle".to_string())
            .children(HistoryScope::ALL.iter().copied().map(|scope| {
                let selected = scope == current;
                let element_id = match scope {
                    HistoryScope::All => "history-scope-all",
                    HistoryScope::ThisWorktree => "history-scope-worktree",
                };
                div()
                    .id(element_id)
                    .debug_selector(move || element_id.to_string())
                    .flex_none()
                    .cursor_pointer()
                    .px(px(7.0))
                    .py(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .border_1()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .when(selected, |el| {
                        el.bg(theme::history::SCOPE_ON_BG)
                            .border_color(theme::history::SCOPE_ON_BORDER)
                            .text_color(theme::history::SCOPE_ON_FG)
                    })
                    .when(!selected, |el| {
                        el.border_color(gpui::transparent_black())
                            .text_color(theme::history::SCOPE_OFF_FG)
                            .hover(|el| el.bg(theme::history::SCOPE_HOVER_BG))
                    })
                    .tooltip(text_tooltip(scope.hint(branch)))
                    .child(scope.label())
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.set_history_scope(scope, cx);
                    }))
            }))
    }

    /// One worktree group's header: the caret, the branch it is, and how many runs are under it.
    ///
    /// The active worktree carries [`theme::border::SELECTED_EDGE`] - §6's "blue edge" - which is
    /// the same 2px edge the rail's own selected worktree row paints, read from the same token so
    /// the two views cannot disagree about which checkout you are in. Every other row reserves
    /// the 2px transparently, for the reason the rail's childless rows reserve their caret slot:
    /// a row that omits it starts 2px left of every other row in the column.
    fn render_history_group(
        &self,
        group: &RunGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let path = group.worktree.clone();
        let was_open = group.open;
        let element_id = gpui::SharedString::from(format!("history-group-{}", path.display()));
        let selector = element_id.clone();
        let count = group.runs.len();

        div()
            .id(element_id)
            .debug_selector(move || selector.to_string())
            .flex()
            .items_center()
            .gap(px(4.0))
            .h(px(24.0))
            .pl(px(10.0))
            .pr(px(10.0))
            .cursor_pointer()
            .border_l_2()
            .border_color(if group.is_active {
                theme::border::SELECTED_EDGE.into()
            } else {
                gpui::transparent_black()
            })
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .tooltip(text_tooltip(format!(
                "{} \u{2014} {}",
                group.label,
                model::earlier_runs_label(count)
            )))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.toggle_history_group(path.clone(), was_open, cx);
            }))
            .child(
                div()
                    .flex_none()
                    .text_color(theme::text::DIM)
                    .child(render_disclosure_caret(group.open, self.ui_text_size(10.0))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(11.5))
                    .text_color(if group.is_active {
                        theme::text::SELECTED
                    } else {
                        theme::history::GROUP_LABEL
                    })
                    .child(group.label.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOSTER)
                    .child(count.to_string()),
            )
    }

    /// One run row - §3's "two lines each: agent chip · title · duration, then outcome pill ·
    /// drift dot · drift text · finished-at".
    ///
    /// Clicking it opens *that run's* transcript as a centre tab, which is the whole Explorer →
    /// editor pattern §3 is built on.
    ///
    /// A run whose drift has not been answered yet paints **no** dot and no drift text, rather
    /// than the reassuring `at the tip` - see [`crate::run_history::model::RunEntry::drift`].
    fn render_history_run_row(
        &self,
        entry: &RunEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let run = &entry.run;
        let now_unix = crate::run_history::unix_now();
        let outcome = entry.outcome();
        let key = run.key.clone();
        let worktree = run.worktree.clone();
        let is_open = self.run_tab_by_worktree.get(&worktree) == Some(&key);

        let element_id = gpui::SharedString::from(format!("history-run-{key}"));
        let body_id = gpui::SharedString::from(format!("history-run-body-{key}"));
        let selector = element_id.clone();

        let mut second_line = div().flex().items_center().gap(px(6.0)).pl(px(21.0)).child(
            // The outcome pill: §5's own four words, in §5's own four colour pairs.
            div()
                .flex_none()
                .px(px(5.0))
                .py(px(1.0))
                .rounded(theme::radius::CHIP)
                .bg(outcome.bg())
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(9.0))
                .text_color(outcome.fg())
                .child(outcome.label()),
        );
        if let (Some(band), Some(commits)) = (entry.band(), entry.drift) {
            second_line = second_line
                .child(
                    div()
                        .flex_none()
                        .w(px(4.0))
                        .h(px(4.0))
                        .rounded_full()
                        .bg(band.dot()),
                )
                .child(
                    div()
                        .flex_none()
                        .font(font(theme::font::SANS))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(band.text())
                        .child(model::drift_label(commits)),
                );
        }
        second_line = second_line.child(div().flex_1().min_w(px(2.0))).child(
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(9.0))
                .text_color(theme::text::GHOST)
                .child(model::run_when(model::run_finished_at(run), now_unix)),
        );

        let mut first_line = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(self.render_agent_chip_icon(
                ProcessKind::Agent(run.kind),
                px(15.0),
                self.ui_text_size(9.0),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(11.5))
                    .text_color(if is_open {
                        theme::text::SELECTED
                    } else {
                        theme::history::ROW_TITLE
                    })
                    .child(model::run_title(run)),
            );
        if let Some(duration) = model::run_duration(run) {
            first_line = first_line.child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOST)
                    .child(duration),
            );
        }

        div()
            .id(element_id)
            .debug_selector(move || selector.to_string())
            .flex()
            .pl(px(13.0))
            // The same connector-then-content shape a rail agent row uses under its worktree, so
            // a run reads as belonging to the group above it rather than as a sibling of it.
            .child(div().flex_none().w(px(1.0)).bg(theme::border::ZONE))
            .child(
                div()
                    .id(body_id)
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .cursor_pointer()
                    .pl(px(7.0))
                    .pr(px(10.0))
                    .pt(px(6.0))
                    .pb(px(7.0))
                    .gap(px(2.0))
                    .border_l_2()
                    .border_color(if is_open {
                        theme::border::SELECTED_EDGE.into()
                    } else {
                        gpui::transparent_black()
                    })
                    .hover(|el| el.bg(theme::surface::ROW_HOVER))
                    .tooltip(text_tooltip(format!(
                        "{} \u{2014} open this run's transcript",
                        model::run_meta_line(run, now_unix)
                    )))
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        this.open_run_tab(worktree.clone(), key.clone(), window, cx);
                    }))
                    .child(first_line)
                    .child(second_line),
            )
    }

    /// The branch name of the checkout the window is on, for the copy that names it (`No agent
    /// has run in <branch> yet.`, and the `this worktree` segment's own tooltip). `None` on a
    /// detached `HEAD`, where there is genuinely no branch name to print - the same honesty
    /// [`crate::rail::strip::problems_empty_note`] keeps one module over.
    fn current_worktree_branch_label(&self) -> Option<String> {
        let path = self.current_worktree_path()?;
        self.repos
            .iter()
            .flat_map(|repo| repo.worktrees.iter())
            .find(|item| item.path == path)
            .and_then(|item| item.branch.clone())
    }
}
