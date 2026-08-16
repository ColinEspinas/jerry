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
        // `run_tab_active` too, not just the map: `run_tab_by_worktree` remembers which run a
        // worktree's tab would *reopen to* (so `render_run_tab` can still find it - see
        // `Self::leave_run_tab`, which clears `run_tab_active` but deliberately leaves this entry
        // alone), not whether that tab is the centre pane's current occupant. Reading the map
        // alone left this row highlighted after switching away to an agent (or the review/graph
        // tab) - a live user report ("history rows weren't unselected when switching away").
        let is_open = self.run_tab_active && self.run_tab_by_worktree.get(&worktree) == Some(&key);

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

/// Real-window, real-git coverage for the History surface (GitHub issue #227): the overflow row
/// that reaches it, the rows it really paints for real persisted records, the transcript tab a
/// row really opens, and the rail line that replaced the inline `HISTORY` section.
///
/// Everything here drives the same entry points the UI does - `open_history_view`,
/// `open_run_tab`, `select_agent` - rather than poking state directly, so it proves the wiring
/// and not just the pure model underneath it (which `crate::run_history::model`'s own tests
/// already hold without a window).
#[cfg(test)]
mod history_surface_tests {
    use super::*;
    use crate::hooks::store::LiveRun;
    use crate::rail::status::Status;
    use crate::root::focus::palette_focus_tests;
    use crate::test_support::{temp_repo_with, TempRoot};
    use crate::work_surface::agents::AgentKind;
    use crate::work_surface::state::TabRef;
    use gpui::TestAppContext;
    use std::path::Path;

    fn init_repo() -> TempRoot {
        temp_repo_with(|root| {
            test_support::seed_empty_repo_at(root);
            test_support::commit(root, "base.txt", "base\n", "initial");
        })
    }

    /// Files one real, *finished* run into the real status store, exactly the way
    /// `crate::hooks::flow::AdeApp::record_agent_statuses` and
    /// `crate::run_history::flow::AdeApp::finish_run_record` do between them, and returns its key.
    fn record_finished_run(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        worktree: &Path,
        spawned_at: i64,
        title: &str,
    ) -> String {
        let key = crate::review::state::baseline_key(worktree, AgentKind::Claude, spawned_at);
        app.update(cx, |app, cx| {
            app.agent_status_state.set(
                key.clone(),
                LiveRun::new(worktree, "Claude", spawned_at, Status::Review)
                    .title(title.to_owned())
                    .session_id(format!("session-{spawned_at}")),
                spawned_at + 100,
            );
            app.agent_status_state.finish(
                &key,
                spawned_at + 360,
                crate::hooks::store::FinishedRun {
                    status: Some(Status::Review),
                    files_changed: Some(2),
                    insertions: Some(41),
                    deletions: Some(0),
                },
            );
            // The real recorder (`crate::hooks::flow::AdeApp::record_agent_statuses`) always
            // notifies; writing straight into the store here has to do the same or the next
            // `run_until_parked` paints the frame before this run existed.
            cx.notify();
        });
        cx.run_until_parked();
        key
    }

    /// The `⋯` overflow's own `History` row really lands on a real, painted History body - the
    /// only entry point §4t leaves it, so a row that switched nothing would be the whole feature
    /// unreachable.
    #[gpui::test]
    fn the_overflow_history_row_really_opens_a_painted_history_view(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        record_finished_run(
            &app,
            cx,
            repo.path(),
            1_700_000_000,
            "Reproduce the refresh race",
        );

        assert!(
            cx.debug_bounds("sidebar-history").is_none(),
            "premise: the sidebar starts on Worktrees"
        );

        app.update_in(cx, |app, window, cx| {
            app.run_rail_menu_action(crate::rail::menu::RailMenuAction::OpenHistory, window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.sidebar_view),
            crate::rail::strip::SidebarView::History
        );
        assert!(
            cx.debug_bounds("sidebar-history").is_some(),
            "the History body must really paint - \u{a7}7 rule 1: ship the affordance with the \
             behaviour"
        );
        assert!(
            cx.debug_bounds("history-scope-toggle").is_some(),
            "\u{a7}6's all / this worktree toggle is part of the view, not an extra"
        );
    }

    /// A real persisted record becomes a real row, and clicking it opens *that run's* transcript
    /// as a real centre tab - §3's Explorer → editor pattern, end to end.
    #[gpui::test]
    fn a_real_run_row_opens_its_own_transcript_tab(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let key = record_finished_run(
            &app,
            cx,
            repo.path(),
            1_700_000_000,
            "Move the select builder behind a trait",
        );

        let tree = app.read_with(cx, |app, _| app.history_run_tree());
        assert_eq!(
            tree.total(),
            1,
            "the real record must produce exactly one row"
        );
        let entry = &tree.repos[0].groups[0].runs[0];
        assert_eq!(
            model::run_title(&entry.run),
            "Move the select builder behind a trait",
            "the row's title is the run's own first prompt"
        );
        assert_eq!(
            entry.outcome(),
            model::Outcome::Done,
            "a watched ending on Review is `done`"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_history_view(cx);
            app.open_run_tab(repo.path().to_path_buf(), key.clone(), window, cx);
        });
        cx.run_until_parked();

        assert!(app.read_with(cx, |app, _| app.run_tab_active));
        assert!(
            app.read_with(cx, |app, _| app.combined_tab_order())
                .contains(&TabRef::Run),
            "the run tab must be a real member of this worktree's strip"
        );
        assert!(
            cx.debug_bounds("run-view").is_some(),
            "and the centre pane must really be showing it"
        );
        for selector in [
            "run-header",
            "run-header-meta",
            "run-outcome-pill",
            "run-transcript",
            "run-footer",
            "run-resume",
            "run-start-new",
        ] {
            assert!(
                cx.debug_bounds(selector).is_some(),
                "\u{a7}3's transcript tab is header + dimmed body + footer with both actions; \
                 `{selector}` is missing"
            );
        }
    }

    /// Live user report: "agents and agents history rows were not unselected when switching
    /// between them when needed." An agent row and a history run row both derived their own
    /// "am I the selected one" from a piece of state nothing ever cleared the *other* row's own
    /// state for, so switching from one to the other could leave both reading as selected at
    /// once - two rail rows (and two tab-strip tabs) both drawn as active.
    ///
    /// Asserted the same way `nothing_selected_means_nothing_shown_anywhere` (`crate::rail::
    /// render`) already asserts single-selection consistency: by reading the exact fields the
    /// render code itself branches on - `AdeApp::active_agent_pane_id` (what
    /// `crate::rail::render::AdeApp::render_agent_row` and `Self::render_agent_tab` both call for
    /// their own `is_selected`/`is_active`) and the `run_tab_active` + `run_tab_by_worktree`
    /// pair (`crate::run_history::render::AdeApp::render_history_run_row`'s own `is_open`) -
    /// rather than trying to read painted colour back out of a window, which this codebase's own
    /// `gpui::TestAppContext` has no way to do (only `debug_bounds`, a geometry lookup).
    #[gpui::test]
    fn switching_between_an_agent_and_a_history_run_leaves_exactly_one_selected(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // A real second agent (the startup shell is `Agents`' first entry) so this exercises a
        // genuine "agent row" rather than the guaranteed startup shell.
        app.update_in(cx, |app, window, cx| {
            app.new_agent(ProcessKind::Agent(AgentKind::Claude), window, cx)
        });
        cx.run_until_parked();
        let agent_id = app.read_with(cx, |app, _| {
            app.agents.iter().last().expect("a spawned agent").id
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.active_agent_pane_id()),
            Some(agent_id),
            "premise: the freshly spawned agent is the one genuinely selected right now"
        );

        let key = record_finished_run(&app, cx, repo.path(), 1_700_000_000, "an earlier run");

        // The real click path: open the History sidebar, then open the run's own transcript tab
        // - exactly what `render_history_run_row`'s `on_click` does.
        app.update_in(cx, |app, window, cx| {
            app.open_history_view(cx);
            app.open_run_tab(repo.path().to_path_buf(), key.clone(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.run_tab_active, "the run tab must now be the active one");
            let history_row_selected =
                app.run_tab_active && app.run_tab_by_worktree.get(repo.path()) == Some(&key);
            assert!(
                history_row_selected,
                "and the history row for the run just opened must read as selected"
            );
            assert_eq!(
                app.active_agent_pane_id(),
                None,
                "the agent row/tab must no longer read as selected - the centre pane is showing \
                 the run transcript, not that agent's pane. Before the fix this was still \
                 `Some(agent_id)`, because both the rail row and the tab strip read \
                 `Agents::active_id` straight, which `open_run_tab` never touches"
            );
            assert_eq!(
                app.agents.active_id(),
                Some(agent_id),
                "the *underlying* remembered agent must be untouched, though - it's what \
                 `select_agent` returns to, not something `open_run_tab` should ever clear"
            );
        });

        // Switch back to the agent - the real click path (`render_agent_row`'s/`render_agent_tab`'s
        // own `on_click`, both of which call `select_agent`).
        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_id, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.active_agent_pane_id(),
                Some(agent_id),
                "the agent row/tab must read as selected again"
            );
            assert!(
                !app.run_tab_active,
                "and the run tab must no longer be the centre pane's active occupant"
            );
            let history_row_selected =
                app.run_tab_active && app.run_tab_by_worktree.get(repo.path()) == Some(&key);
            assert!(
                !history_row_selected,
                "so the history row must read as unselected too - even though \
                 `run_tab_by_worktree` still remembers this run for this worktree (that's what \
                 lets the run tab in the strip still resolve to it if re-activated), the row's own \
                 selection reads `run_tab_active` first and that is genuinely `false` now"
            );
        });
    }

    /// §3: "One run tab per worktree; opening another replaces it." The replacement is a
    /// replacement, not a second tab - and it does not touch another checkout's own open tab.
    #[gpui::test]
    fn a_second_run_replaces_the_tab_rather_than_stacking_beside_it(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let first = record_finished_run(&app, cx, repo.path(), 1_700_000_000, "first");
        let second = record_finished_run(&app, cx, repo.path(), 1_700_001_000, "second");
        let elsewhere = Path::new("/other/checkout");
        let other = record_finished_run(&app, cx, elsewhere, 1_700_002_000, "elsewhere");

        app.update_in(cx, |app, window, cx| {
            app.open_run_tab(repo.path().to_path_buf(), first.clone(), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(app.read_with(cx, |app, _| app.open_run_key()), Some(first));

        app.update_in(cx, |app, window, cx| {
            app.open_run_tab(repo.path().to_path_buf(), second.clone(), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.open_run_key()),
            Some(second.clone())
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.combined_tab_order())
                .iter()
                .filter(|tab| **tab == TabRef::Run)
                .count(),
            1,
            "opening another run replaces the tab; it never stacks a second one"
        );

        // Another checkout's run tab is its own - opening one there must leave this one alone.
        app.update_in(cx, |app, window, cx| {
            app.open_run_tab(elsewhere.to_path_buf(), other, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .run_tab_by_worktree
                .get(repo.path())
                .cloned()),
            Some(second),
            "\u{a7}3 keys the tab to a worktree: opening a run in another checkout must not \
             close the one you were reading here"
        );
    }

    /// The `all` scope lists every checkout's runs, so a row click very often names a worktree
    /// you are not standing in - and the tab it opens lives in *that* worktree's strip. Opening
    /// one must therefore select its checkout, or the tab is filed somewhere nothing is drawing.
    ///
    /// A real regression test: before the fix, this rendered an empty tab strip and a centre pane
    /// reading "this run is no longer in the history", because `open_run_key` resolves against the
    /// selected worktree. Caught by a screenshot of the running app.
    #[gpui::test]
    fn opening_another_checkouts_run_selects_that_checkout(cx: &mut TestAppContext) {
        let repo = init_repo();
        let wt = temp_repo_with(|_| {});
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            cx.notify();
            app.worktrees.push(crate::rail::worktrees::WorktreeItem {
                path: wt.path().to_path_buf(),
                label: "feature-a".to_string(),
                branch: Some("feature-a".to_string()),
                is_main: false,
                is_bare: false,
                is_detached: false,
                short_sha: None,
                is_locked: false,
                lock_reason: None,
                is_broken: false,
                broken_reason: None,
                error: None,
            });
        });
        let key = record_finished_run(&app, cx, wt.path(), 1_700_000_000, "in another checkout");
        assert_ne!(
            app.read_with(cx, |app, _| app.current_worktree_path()),
            Some(wt.path().to_path_buf()),
            "premise: the window is standing somewhere else"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_run_tab(wt.path().to_path_buf(), key.clone(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.current_worktree_path()),
            Some(wt.path().to_path_buf()),
            "opening a run selects its own checkout"
        );
        assert_eq!(app.read_with(cx, |app, _| app.open_run_key()), Some(key));
        assert!(app.read_with(cx, |app, _| app.run_tab_active));
        assert!(
            app.read_with(cx, |app, _| app.combined_tab_order())
                .contains(&TabRef::Run),
            "and the tab really lands in the strip that is now on screen"
        );
        assert!(cx.debug_bounds("run-view").is_some());
    }

    /// The other half of the same rule: switching worktrees *leaves* the run tab, so the centre
    /// pane never keeps painting another checkout's recording - or, worse, a "no longer in the
    /// history" note for a worktree that simply has no run tab of its own.
    #[gpui::test]
    fn switching_worktrees_leaves_the_run_tab_behind_in_its_own_strip(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let key = record_finished_run(&app, cx, repo.path(), 1_700_000_000, "a run");
        app.update_in(cx, |app, window, cx| {
            app.open_run_tab(repo.path().to_path_buf(), key.clone(), window, cx);
        });
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.run_tab_active), "premise");

        let index = app
            .read_with(cx, |app, _| {
                app.worktrees
                    .iter()
                    .position(|item| item.path == repo.path())
            })
            .expect("the main checkout is a worktree");
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(index, window, cx);
        });
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.run_tab_active),
            "a worktree switch leaves the surface"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app
                .run_tab_by_worktree
                .get(repo.path())
                .cloned()),
            Some(key),
            "but the tab itself is untouched - it is one switch back, in its own strip"
        );
    }

    /// §7's real cost, in this app's own terms: the run tab occupies the centre pane, so
    /// selecting an agent while it is showing must genuinely *leave* it - not merely stop drawing
    /// it while `Window::focus` still points at a handle nothing is tracking.
    #[gpui::test]
    fn selecting_an_agent_really_leaves_the_run_tab(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let key = record_finished_run(&app, cx, repo.path(), 1_700_000_000, "a run");

        app.update_in(cx, |app, window, cx| {
            app.open_run_tab(repo.path().to_path_buf(), key, window, cx);
        });
        cx.run_until_parked();
        assert!(app.read_with(cx, |app, _| app.run_tab_active), "premise");
        assert!(app.read_with(cx, |app, _| app.centre_pane_is_not_an_agent()));

        let agent = app
            .read_with(cx, |app, _| app.agents.iter().next().map(|agent| agent.id))
            .expect("every window starts with one shell");
        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent, window, cx);
        });
        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.run_tab_active),
            "the run tab must be left, so the agent tab being switched to really mounts"
        );
        assert!(
            !app.read_with(cx, |app, _| app.centre_pane_is_not_an_agent()),
            "and the one shared predicate \u{a7}7 asks for must agree"
        );
        assert!(
            cx.debug_bounds("run-view").is_none(),
            "nothing of the run surface may still be painted"
        );
    }

    /// §6's rail line, and the deletion that came with it: a worktree with real history and **no
    /// live agent** gets a `↺ N earlier runs` line and no disclosure caret - the caret only ever
    /// meant "this worktree has children", and history is no longer one of them.
    ///
    /// The line is the *whole* rail-side surface for history now. The inline `HISTORY` section it
    /// replaced - a label plus one `Resume`/`Reopen` row per run - is deleted, per §7 rule 5.
    #[gpui::test]
    fn a_worktree_with_history_and_no_agent_gets_the_earlier_runs_line(cx: &mut TestAppContext) {
        let repo = init_repo();
        let wt = temp_repo_with(|_| {});
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            cx.notify();
            app.worktrees = vec![crate::rail::worktrees::WorktreeItem {
                path: wt.path().to_path_buf(),
                label: "feature-a".to_string(),
                branch: Some("feature-a".to_string()),
                is_main: false,
                is_bare: false,
                is_detached: false,
                short_sha: None,
                is_locked: false,
                lock_reason: None,
                is_broken: false,
                broken_reason: None,
                error: None,
            }];
        });
        record_finished_run(&app, cx, wt.path(), 1_700_000_000, "an earlier run");
        cx.run_until_parked();

        // `debug_bounds` takes a `&'static str`, so the selector is leaked for the test's own
        // lifetime - the only way to look up a bounds key derived from a temp dir's real path.
        let link: &'static str =
            Box::leak(format!("earlier-runs-{}", wt.path().display()).into_boxed_str());
        assert!(
            cx.debug_bounds(link).is_some(),
            "\u{a7}6: a worktree with no live agent carries its own `\u{21ba} N earlier runs` line"
        );
        assert!(
            cx.debug_bounds("worktree-caret-0").is_some(),
            "the caret *slot* is still reserved - it is 13px of every row's left inset"
        );
        // ...but history is no longer one of the row's children, so nothing about it can make the
        // row expandable. That is the other half of the deletion: the inline `HISTORY` section
        // used to live behind this caret, and `has_children` had to be widened for it.
        let row = app.update(cx, |app, cx| {
            app.build_repo_groups(cx)
                .into_iter()
                .flat_map(|group| group.rows)
                .find(|row| row.path == wt.path())
                .expect("the worktree row")
        });
        assert!(
            row.agents.is_empty(),
            "premise: no live agent in this checkout"
        );
        assert!(
            !row.history.is_empty(),
            "premise: it does have real persisted history"
        );

        // Clicking it lands on History, scoped to that checkout - §6's "switching the sidebar to
        // History **for that worktree**".
        app.update_in(cx, |app, window, cx| {
            app.open_history_for_worktree(wt.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.sidebar_view),
            crate::rail::strip::SidebarView::History
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.history_scope),
            HistoryScope::ThisWorktree
        );
        assert!(cx.debug_bounds("sidebar-history").is_some());
    }

    /// Screenshot-reported: "the earlier run row spacing in the worktree pane is wrong, it should
    /// be correctly centered - like [the `main checkout \u{b7} clean` row]". Real root cause,
    /// found with `debug_bounds` against this exact row before the fix: `render_earlier_runs_link`
    /// used to add `trailing_pb`'s `.pb(px(7.0))` straight onto the same `div` that carries a
    /// fixed `.h(px(19.0))`. `taffy`'s default `BoxSizing::BorderBox` (the same rule
    /// `Self::render_worktree_row`'s own `header` note documents) means that padding ate into the
    /// row's own 19px content area rather than adding space below it - real measured bounds on the
    /// unfixed code showed both the `\u{21ba}` glyph and the `N earlier runs` label centered
    /// 3.5px *above* the row's own vertical center, spilling 1.5px above the row's own top edge.
    ///
    /// The fix moved the 7px into a real sibling spacer box (the same idiom
    /// `Self::render_worktree_row`/`Self::render_repo_group_header` already use), leaving `row`'s
    /// own 19px content area untouched. This test proves the real, painted result: the icon and
    /// label glyphs' own vertical centers land on the row's own vertical center - and, so this is
    /// not just an accident of this one row's numbers, that the exact sibling row the user's own
    /// screenshot called out as already correct - the worktree row's own `\u{b7} <note>` text,
    /// e.g. `main checkout \u{b7} clean` (`Self::render_worktree_row`) - centers its own text the
    /// same way: on its own row's real geometric center, not the note text's own.
    #[gpui::test]
    fn earlier_runs_link_icon_and_label_are_centered_like_the_worktree_rows_own_note(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let wt = temp_repo_with(|_| {});
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            cx.notify();
            app.worktrees = vec![crate::rail::worktrees::WorktreeItem {
                path: wt.path().to_path_buf(),
                label: "feature-a".to_string(),
                branch: Some("feature-a".to_string()),
                is_main: false,
                is_bare: false,
                is_detached: false,
                short_sha: None,
                is_locked: false,
                lock_reason: None,
                is_broken: false,
                broken_reason: None,
                error: None,
            }];
        });
        // A real finished run, so this worktree really carries a nonzero `history.len()` and its
        // `EarlierRunsLink` row genuinely renders right below its own `WorktreeRow` header - the
        // same seeding the sibling test above this one uses. No live agent, so the header's own
        // `\u{b7} <note>` sibling text renders too (`when(!has_agents, ..)`).
        record_finished_run(&app, cx, wt.path(), 1_700_000_000, "an earlier run");
        cx.run_until_parked();

        let center = |b: gpui::Bounds<gpui::Pixels>| -> f32 {
            f32::from(b.origin.y) + f32::from(b.size.height) / 2.0
        };

        // The earlier-runs-link row: its own row bounds, and its icon glyph's and label's bounds
        // (selectors added alongside the fix so a real test can reach past the row and measure
        // its own children, the same `debug_selector` idiom every other rail row test in this
        // crate already relies on).
        let link_row_sel: &'static str =
            Box::leak(format!("earlier-runs-{}", wt.path().display()).into_boxed_str());
        let link_icon_sel: &'static str =
            Box::leak(format!("earlier-runs-icon-{}", wt.path().display()).into_boxed_str());
        let link_label_sel: &'static str =
            Box::leak(format!("earlier-runs-label-{}", wt.path().display()).into_boxed_str());
        let link_row = cx
            .debug_bounds(link_row_sel)
            .expect("the earlier-runs-link row must paint");
        let link_icon = cx
            .debug_bounds(link_icon_sel)
            .expect("its icon glyph must paint");
        let link_label = cx
            .debug_bounds(link_label_sel)
            .expect("its label must paint");

        let link_row_center = center(link_row);
        let icon_offset = center(link_icon) - link_row_center;
        let label_offset = center(link_label) - link_row_center;

        // The real regression: both the icon and the label must be centered on the row's own
        // real vertical center, not 3.5px above it, and neither may spill above the row's own
        // top edge the way the unfixed code did.
        assert!(
            icon_offset.abs() < 0.5,
            "the \u{21ba} glyph must be vertically centered in its 19px row; \
             real offset from the row's own center was {icon_offset}px \
             (row={link_row:?}, icon={link_icon:?})"
        );
        assert!(
            label_offset.abs() < 0.5,
            "the 'N earlier runs' label must be vertically centered in its 19px row; \
             real offset from the row's own center was {label_offset}px \
             (row={link_row:?}, label={link_label:?})"
        );
        assert!(
            link_icon.origin.y >= link_row.origin.y,
            "the icon must not spill above the row's own top edge - it did before the fix \
             (icon top {:?} vs. row top {:?})",
            link_icon.origin.y,
            link_row.origin.y
        );

        // The real sibling comparison: worktree row 0's own header is the exact row the user's
        // screenshot showed as already correct (`main checkout \u{b7} clean`) - its `\u{b7} <note>`
        // text must center on *its* row's real vertical center exactly the same way, so the
        // earlier-runs-link row's centering isn't a coincidence of its own numbers, it matches
        // this app's one real convention for a single-line, fixed-height rail row.
        let worktree_row_sel: &'static str =
            Box::leak(format!("worktree-row-0-{}", wt.path().display()).into_boxed_str());
        let worktree_row = cx
            .debug_bounds(worktree_row_sel)
            .expect("the real worktree row must paint");
        let worktree_note = cx
            .debug_bounds("worktree-row-note-0")
            .expect("its own \u{b7} note text must paint (no live agent)");
        let note_offset = center(worktree_note) - center(worktree_row);

        assert!(
            (icon_offset - note_offset).abs() < 0.5,
            "the earlier-runs-link icon and the worktree row's own note text must share the same \
             real vertical-center offset within their own row - earlier-runs offset {icon_offset}px, \
             worktree-row-note offset {note_offset}px"
        );
        assert!(
            (label_offset - note_offset).abs() < 0.5,
            "the earlier-runs-link label and the worktree row's own note text must share the same \
             real vertical-center offset within their own row - earlier-runs offset {label_offset}px, \
             worktree-row-note offset {note_offset}px"
        );
    }

    /// The two empty states are two different facts, and the view says which one it is - the same
    /// pair the Problems view one module over keeps, for the same reason.
    #[gpui::test]
    fn no_history_and_no_match_are_two_different_notes(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, _window, cx| app.open_history_view(cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-history-note").is_some(),
            "a window with no runs at all says so"
        );
        assert!(cx.debug_bounds("sidebar-history-count").is_none());

        record_finished_run(&app, cx, repo.path(), 1_700_000_000, "Bump axum to 0.8");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("sidebar-history-count").is_some(),
            "one real run earns a real count line"
        );

        app.update(cx, |app, cx| {
            app.filter_query
                .set("nothing matches this", std::time::Instant::now());
            cx.notify();
        });
        cx.run_until_parked();
        let tree = app.read_with(cx, |app, _| app.history_run_tree());
        assert!(tree.is_empty());
        assert_eq!(
            tree.unfiltered, 1,
            "the unfiltered count is what tells 'no history' from 'no match', and the note reads \
             off it"
        );
        assert_eq!(
            model::filtered_away_note(tree.unfiltered),
            "No match in the 1 run."
        );
    }
}
