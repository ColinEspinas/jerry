use super::*;
use crate::root::plural;
use crate::root::scrollbar;
use crate::root::widgets::{render_disclosure_caret, text_tooltip, SimpleInput};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How far past [`AdeApp::rail_list_state`]'s own viewport `Self::render_rail_list`'s `gpui::list`
/// measures rows ahead of time - the same real overdraw margin
/// `crate::sidebar::render::CHANGES_LIST_OVERDRAW` uses for the identical "a little slack so a
/// small scroll doesn't have to measure a brand new row synchronously" reason.
pub(crate) const RAIL_LIST_OVERDRAW: gpui::Pixels = px(48.0);

/// The agent row's line-2 state word (§2.3) - deliberately distinct from [`Status::label`]
/// (`"Idle"`, used everywhere else this enum shows text, e.g. the work-surface context bar):
/// only the rail agent row uses `"paused"` for [`Status::Idle`], since that's the one place the
/// design gives it a different word ("needs input / failed / running / finished / paused" - no
/// "idle" appears in that list at all). Changing [`Status::label`] itself to match would have
/// renamed it everywhere else in the app too.
fn agent_state_word(status: Status) -> &'static str {
    match status {
        Status::Ask => "needs input",
        Status::Fail => "failed",
        Status::Review => "finished",
        Status::Run => "running",
        Status::Idle => "paused",
    }
}

/// How many agents in this window are waiting on a human - the Worktrees cell's state marker
/// (GitHub issue #291).
fn agents_needing_you(rows: &[AgentRow]) -> usize {
    rail::urgency_counts(rows)
        .into_iter()
        .filter(|(status, _)| matches!(status, Status::Ask | Status::Fail))
        .map(|(_, count)| count)
        .sum()
}

/// The agent row's line-2 trailing text (§2.3's exact per-status table): empty for `needs
/// input` (the dot and state word are the whole message), the live activity for `running`, the
/// exit code for `failed`, the review's file count for `finished`, and `resumable · Nh` for
/// `paused`.
fn agent_trailing_text(agent: &AgentRow) -> String {
    match agent.status {
        Status::Ask => String::new(),
        Status::Run => agent.activity.clone().unwrap_or_default(),
        Status::Fail => agent
            .exit_code
            .map(|code| format!("exit {code}"))
            .unwrap_or_default(),
        Status::Review => agent
            .review_file_count
            .map(|count| plural::count(count, "file", None))
            .unwrap_or_default(),
        Status::Idle => format!("resumable \u{b7} {}", rail::format_elapsed(agent.elapsed)),
    }
}

/// An agent row's title colour (`STAGE-A-CHANGELOG.md` §4n, `Jerry.dc.html`'s own `titleFg`).
fn agent_title_color(status: Status, is_selected: bool) -> theme::ColorToken {
    if is_selected {
        theme::text::SELECTED
    } else if status == Status::Idle {
        theme::text::DIMMER
    } else {
        theme::rail::AGENT_TITLE
    }
}

/// A worktree row's 2px left edge: **selection or nothing**.
fn worktree_row_edge(
    _aggregate_status: Status,
    _is_prunable: bool,
    is_selected: bool,
) -> Option<theme::ColorToken> {
    is_selected.then_some(theme::border::SELECTED_EDGE)
}

/// Which of the repo header's **two** urgency counts a dot+count pair is
/// (`design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §4: "`● 2` amber... needs input
/// and `● 1` red... failed").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UrgencyCount {
    /// Worktrees needing input - amber.
    NeedsInput,
    /// Worktrees holding a failed agent - red. A worktree in both states is counted only here
    /// (see [`rail::RepoGroup::needs_input_count`]).
    Failed,
}

impl UrgencyCount {
    /// The 5px dot's fill - the same two status hues the title bar's own `● 2  ● 1  ● 4` cluster
    /// and a worktree row's per-agent dots use, which is exactly why §4q could drop the sentence:
    /// "Replaced with the rail's existing vocabulary."
    fn dot(self) -> theme::ColorToken {
        match self {
            UrgencyCount::NeedsInput => theme::status::ASK,
            UrgencyCount::Failed => theme::status::FAIL,
        }
    }

    /// The count's own text colour - the desaturated partner of [`Self::dot`], per §4's
    /// `#e2a336` dot / `#c99b4e` text and `#e0625c` dot / `#c4726d` text pairs.
    fn text(self) -> theme::ColorToken {
        match self {
            UrgencyCount::NeedsInput => theme::rail::REPO_ASK_COUNT,
            UrgencyCount::Failed => theme::rail::REPO_FAIL_COUNT,
        }
    }

    /// This pair's own tooltip sentence (§4: "each with its own tooltip").
    fn tooltip(self, count: usize) -> String {
        match self {
            UrgencyCount::NeedsInput => rail::needs_input_tooltip(count),
            UrgencyCount::Failed => rail::failed_tooltip(count),
        }
    }

    /// The debug-selector suffix a test locates this pair by.
    fn selector_name(self) -> &'static str {
        match self {
            UrgencyCount::NeedsInput => "ask",
            UrgencyCount::Failed => "fail",
        }
    }
}

impl AdeApp {
    /// One of the repo header's two urgency dot+count pairs, or **nothing at all** at zero
    /// (`REVISION-2026-08-14.md` §4: "Each hidden at zero").
    fn render_repo_urgency_count(
        &self,
        repo_id: repo::RepoId,
        kind: UrgencyCount,
        count: usize,
    ) -> Option<impl IntoElement> {
        if count == 0 {
            return None;
        }
        let name = kind.selector_name();
        Some(
            div()
                // `name` (not one shared literal) keeps the two pairs' element ids distinct
                // within the one header they both render into.
                .id((name, repo_id.0))
                .debug_selector(move || format!("repo-{name}-count-{}", repo_id.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(4.0))
                .tooltip(text_tooltip(kind.tooltip(count)))
                .child(
                    div()
                        .flex_none()
                        .w(px(5.0))
                        .h(px(5.0))
                        .rounded_full()
                        .bg(kind.dot()),
                )
                .child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .font(font(theme::font::MONO))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(self.ui_text_size(9.5))
                        .text_color(kind.text())
                        .child(count.to_string()),
                ),
        )
    }

    /// Types into [`Self::filter_query`] - a small, hand-rolled text field rather than
    /// `vendor/zed/crates/gpui/examples/input.rs`'s full `EntityInputHandler`, judged out of
    /// scope for a single filter row. Modified keystrokes (⌘, ⌃, ⌥) are left unhandled and
    /// keep propagating, so app-level shortcuts (e.g. ⌘N) still reach their bindings while
    /// this field has focus.
    pub(in crate::rail) fn handle_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: the modifier check is `widgets::text_editing_modifiers`' now, not a
        // flat "any of Ctrl/Alt/Cmd means not ours" - word-wise Ctrl+Left/Right (Alt on macOS) is
        // real text editing that this used to refuse, and Shift is the extend-selection modifier
        // rather than something to ignore.
        let Some(modifiers) = widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        // GitHub issue #27's "solid mid-keystroke" - see `crate::palette::render::AdeApp::
        // handle_palette_key_down`'s identical reasoning.
        self.reset_caret_blink(cx);
        let changed = match keystroke.key.as_str() {
            // A real, undoable step, not a silent loss: `Esc` clearing a typed filter is exactly
            // the case Ctrl+Z should bring back. See `crate::text_history::TextField::set`.
            "escape" => self.filter_query.clear(Instant::now()),
            key => self.filter_query.handle_editing_key(
                key,
                keystroke.key_char.as_deref(),
                modifiers,
                Instant::now(),
            ),
        };
        if changed {
            self.prune_confirm_armed = false;
            self.discard_confirm_armed = None;
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// `TextUndo`/`TextRedo` for the rail's filter field (GitHub issue #17) - see
    /// `crate::default_key_bindings`' own docs for the scoping, and
    /// `crate::text_history::TextField` for the history itself.
    pub(in crate::rail) fn handle_filter_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_query.undo() {
            cx.notify();
        }
    }

    pub(in crate::rail) fn handle_filter_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filter_query.redo() {
            cx.notify();
        }
    }

    /// Builds the rail's per-agent rows from live state: each agent's `TerminalPane`
    /// (process signal, question preview), the matching worktree's branch name, and the diff
    /// summary from [`Self::diff_cache`] (refreshed by the periodic task started in
    /// `Self::new`). An agent with no diff data yet simply shows `0`/`0` until the next
    /// status-poll tick fills it in. Iterates [`Self::agents`] with no repo filter of its own -
    /// every currently open agent gets a row here regardless of which repo is focused right now,
    /// which is what lets [`Self::build_repo_groups`] show a background repo's own agents with
    /// genuinely live status rather than nothing at all.
    pub(crate) fn build_agent_rows(&self, cx: &App) -> Vec<AgentRow> {
        self.agents
            .iter()
            .filter(|agent| agent.kind.is_agent_session())
            .map(|agent| {
                let status_value = self.agent_status(agent, cx);
                let pane = agent.pane.read(cx);
                let diff = self.diff_cache.get(&agent.cwd).copied();

                let branch = self
                    .worktrees
                    .iter()
                    .find(|item| item.path == agent.cwd)
                    .or_else(|| {
                        self.repos.iter().find_map(|repo| {
                            repo.worktrees.iter().find(|item| item.path == agent.cwd)
                        })
                    })
                    .and_then(|item| item.branch.clone());

                // GitHub issue #239 phase 2: real, structured text straight from this agent's own
                // hook payloads, when it has fired any recently enough to still be describing the
                // present (`crate::hooks::event::HOOK_SIGNAL_TTL`).
                //
                // Only the *activity* half is read here. The question half used to feed a rail
                // question-preview card, which revision 6 removed outright (see
                // `Self::render_worktree_row`'s own note, and the field this row no longer
                // carries) - it is still recorded, from this same real source, by
                // `crate::hooks::flow::AdeApp::record_agent_statuses`, which is what History and a
                // restored session read it back from.
                let hook_activity = match &self.hook_runtime {
                    Some(runtime) => runtime.text_for(agent.id).0,
                    None => None,
                };

                // Only shown while the agent is actually running: a stale "Bash: cargo test" next
                // to an idle or review-ready row would describe something that already finished.
                let activity = hook_activity.filter(|_| status_value == Status::Run);

                let title = match agent.cwd.file_name() {
                    Some(name) => name.to_string_lossy().into_owned(),
                    None => agent.cwd.display().to_string(),
                };

                // GitHub issue #225: a real per-agent count, read from this agent's *own* review
                // against its *own* baseline (`Self::agent_review_file_count`) - no longer the
                // whole worktree's git diff, which was never this agent's answer and was only
                // ever available for the one worktree currently loaded in Zone 3. It is now real
                // for every single-agent worktree, loaded or not. See that method's own docs for
                // exactly when it stays `None`.
                let review_file_count = if status_value == Status::Review {
                    self.agent_review_file_count(agent.id)
                } else {
                    None
                };

                AgentRow {
                    id: agent.id,
                    kind: agent.kind,
                    title,
                    cwd: agent.cwd.clone(),
                    status: status_value,
                    branch,
                    add: diff.map(|summary| summary.add).unwrap_or(0),
                    del: diff.map(|summary| summary.del).unwrap_or(0),
                    exit_code: pane.exit_status().map(|status| status.exit_code()),
                    activity,
                    elapsed: agent.spawned_at.elapsed(),
                    review_file_count,
                }
            })
            .collect()
    }

    /// Builds one [`WorktreeRow`] per worktree, folding in every currently open agent and (GitHub
    /// issue #227) every real persisted-but-not-currently-running agent
    /// (`crate::rail::state::build_worktree_rows_with_history`) - the single real per-render
    /// source both rail modes now build their list from (see [`Self::render_rail_list`]).
    pub(in crate::rail) fn build_worktree_rows(&self, cx: &App) -> Vec<WorktreeRow> {
        let live_keys = self.live_agent_status_keys();
        let history: Vec<crate::hooks::history::PastAgent> = self
            .worktrees
            .iter()
            .filter(|item| item.error.is_none())
            .flat_map(|item| {
                crate::hooks::history::past_agents_for_worktree(
                    &self.agent_status_state,
                    &item.path,
                    &live_keys,
                )
            })
            .collect();
        rail::build_worktree_rows_with_history(
            &self.build_worktree_entries(),
            &self.build_agent_rows(cx),
            &history,
        )
    }

    /// Builds the worktree list: every worktree `wt_core::list_worktrees` reported, including
    /// ones that failed to read - `crate::rail::worktrees::WorktreeItem`'s docs say a per-entry error
    /// is kept in the list rather than filtered out, and `Self::render_worktree_row` renders an
    /// errored entry as a visible, non-interactive row.
    pub(in crate::rail) fn build_worktree_entries(&self) -> Vec<WorktreeEntry> {
        self.build_worktree_entries_from(&self.worktrees)
    }

    /// [`Self::build_worktree_entries`]'s own logic, generalized to any [`WorktreeItem`] list -
    /// [`Self::build_repo_groups`]'s non-focused-repo rows reuse this against each repo's own
    /// [`crate::rail::repo::Repo::worktrees`] rather than [`Self::worktrees`].
    pub(in crate::rail) fn build_worktree_entries_from(
        &self,
        items: &[WorktreeItem],
    ) -> Vec<WorktreeEntry> {
        items
            .iter()
            .map(|item| {
                if let Some(error) = &item.error {
                    return WorktreeEntry {
                        path: item.path.clone(),
                        label: item.label.clone(),
                        branch: None,
                        note: WorktreeNote {
                            is_main: false,
                            clean: None,
                            merge: None,
                            is_locked: false,
                        },
                        error: Some(error.clone()),
                    };
                }

                let note = self
                    .worktree_notes
                    .get(&item.path)
                    .cloned()
                    .unwrap_or(WorktreeNote {
                        is_main: item.is_main,
                        clean: None,
                        merge: None,
                        is_locked: item.is_locked,
                    });
                WorktreeEntry {
                    path: item.path.clone(),
                    label: item.label.clone(),
                    branch: item.branch.clone(),
                    note,
                    error: None,
                }
            })
            .collect()
    }

    /// Starts the rail's periodic status background refresh (see [`STATUS_POLL_INTERVAL`]'s
    /// docs). Every tick: snapshots the current worktree paths, open agents' cwds, and open
    /// agents' real pids on the foreground thread (cheap, no I/O), computes a
    /// [`rail::StatusSnapshot`] *and* a real [`process_stats::sample_processes`] reading on the
    /// background executor, then writes both results back into
    /// [`Self::diff_cache`]/[`Self::worktree_notes`]/[`Self::ahead_behind_cache`]/
    /// [`Self::process_stats`] on the foreground thread - the same "gather/compute/write back"
    /// shape [`Self::load_worktrees`]/[`Self::load_diff`] use.
    pub(crate) fn start_status_polling(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let mut prev_process_samples: HashMap<u32, process_stats::RawCpuSample> =
                HashMap::new();
            loop {
                cx.background_executor().timer(STATUS_POLL_INTERVAL).await;

                let Ok((worktrees, diff_paths, pids, review_targets)) =
                    this.update(cx, |this, cx| {
                        let worktrees: Vec<rail::WorktreeQuery> = this
                            .worktrees
                            .iter()
                            .filter(|item| item.error.is_none())
                            .map(|item| rail::WorktreeQuery {
                                path: item.path.clone(),
                                is_main: item.is_main,
                                is_locked: item.is_locked,
                            })
                            .collect();
                        let diff_paths: Vec<PathBuf> =
                            this.agents.iter().map(|agent| agent.cwd.clone()).collect();
                        // Every open agent's real pid, plus this process's own: GitHub issue
                        // #293's Resources tree carries Jerry itself as a real row (the bar
                        // readout promises "what Jerry is costing this machine right now", which
                        // a total excluding the window, its editors and its language servers
                        // would not be), and a row with no sample behind it would render a
                        // permanent `...`.
                        let pids: Vec<u32> = this
                            .agents
                            .iter()
                            .filter_map(|agent| agent.pane.read(cx).pid())
                            .chain(std::iter::once(std::process::id()))
                            .collect();
                        // GitHub issue #225: every agent with a captured baseline, to be measured
                        // against it below.
                        let review_targets = this.review_measure_targets();
                        (worktrees, diff_paths, pids, review_targets)
                    })
                else {
                    break;
                };

                let (snapshot, process_samples, next_prev, review_measurements) = cx
                    .background_executor()
                    .spawn(async move {
                        let snapshot = rail::compute_status_snapshot(&worktrees, &diff_paths);
                        let (process_samples, next_prev) =
                            process_stats::sample_processes(&pids, prev_process_samples);
                        // GitHub issue #225: each agent's own unreviewed set, measured against
                        // its own baseline. `changed_paths_against_tree` is one
                        // `git diff --name-only` process per agent with no hunk parsing - the
                        // cheap counterpart to the full review the tab itself loads.
                        //
                        // This runs on the *poll*, not only when the Review tab is open, and that
                        // is load-bearing rather than eager: `Status::Review` is what surfaces the
                        // footer's `Review` door, the door is what opens the tab, and the tab is
                        // what loads the full diff - so measuring only inside the tab would be
                        // circular and nothing would ever become reviewable at all.
                        //
                        // A failed measurement is dropped rather than recorded as an empty set;
                        // see `AdeApp::apply_review_measurements`.
                        let review_measurements: Vec<(
                            crate::work_surface::agents::AgentId,
                            String,
                            Vec<PathBuf>,
                        )> = review_targets
                            .into_iter()
                            .filter_map(|(id, worktree, tree_id, untracked)| {
                                let paths = wt_core::review::changed_paths_against_tree(
                                    &worktree, &tree_id, untracked,
                                )
                                .ok()?;
                                Some((id, tree_id, paths))
                            })
                            .collect();
                        (snapshot, process_samples, next_prev, review_measurements)
                    })
                    .await;
                prev_process_samples = next_prev;

                let updated = this.update(cx, |this, cx| {
                    this.diff_cache = snapshot.diffs;
                    this.worktree_notes = snapshot.worktree_notes;
                    this.ahead_behind_cache = snapshot.ahead_behind;
                    this.process_stats = process_samples;
                    // GitHub issue #293: the Resources popover's `Updated Ns ago` line measures
                    // against *this* instant - the moment a real sample landed - so it stays
                    // honest about staleness even if the poll itself stalls.
                    this.process_stats_sampled_at = Some(Instant::now());
                    this.apply_review_measurements(review_measurements);
                    // GitHub issue #239 phase 2: fold each agent's real, hook-derived state into
                    // the persisted record for issue #227 to build on. Rides this existing timer
                    // rather than adding one, and only touches the disk when something actually
                    // changed - see `AdeApp::record_agent_statuses`.
                    this.record_agent_statuses(cx);
                    // GitHub issue #284: drain the file writes the hook layer has reported since
                    // the last tick and turn them into per-line attribution. A third pass on the
                    // same timer, for the same reason as the two around it - and deliberately a
                    // pass of its own rather than folded into `record_agent_statuses`, whose
                    // "changed" signal is about a *status* and says nothing about whether a file
                    // was written (see `crate::provenance::flow`).
                    this.apply_agent_edits(cx);
                    // GitHub issue #226: detect real agent-status transitions (needs input /
                    // finished with changes to review) and play a sound if the user wants one.
                    // Deliberately a separate pass from `record_agent_statuses` just above, not
                    // folded into it - see `crate::sound::flow`'s own module docs for why that
                    // function's "changed" signal can't be reused here (it only ever sees
                    // Claude Code agents with a fresh hook fact, never Codex).
                    this.play_agent_status_sounds(cx);
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        self._status_poll_task = Some(task);
    }

    /// Starts the worktree panel's live-refresh loop (GitHub issue #12): a real `notify`
    /// filesystem watcher (`crate::rail::worktree_watch::spawn_worktree_watcher`, stored in
    /// [`Self::_worktree_watcher`] to keep it alive) plus the [`WORKTREE_WATCH_POLL_INTERVAL`]
    /// poll fallback the issue asks for, both driving the exact same
    /// [`Self::load_worktrees`] real re-parse - never a separate, divergent code path.
    pub(crate) fn start_worktree_watch(&mut self, cx: &mut Context<Self>) {
        let repo_path = self.focused_repo_path();
        let dirty: worktree_watch::DirtyFlag = Arc::new(AtomicBool::new(false));
        self._worktree_watcher = worktree_watch::spawn_worktree_watcher(&repo_path, dirty.clone());

        let task = cx.spawn(async move |this, cx| {
            let mut last_refresh = Instant::now();
            loop {
                cx.background_executor().timer(WORKTREE_WATCH_TICK).await;

                let watcher_fired = dirty.load(Ordering::SeqCst);
                if watcher_fired {
                    // Let a burst of events from one `git worktree` invocation settle before
                    // acting, then clear whatever accumulated during the settle window too -
                    // it's all being answered by the single refresh about to run either way.
                    cx.background_executor().timer(WORKTREE_WATCH_SETTLE).await;
                    dirty.store(false, Ordering::SeqCst);
                }
                let poll_due = last_refresh.elapsed() >= WORKTREE_WATCH_POLL_INTERVAL;

                if !watcher_fired && !poll_due {
                    continue;
                }
                last_refresh = Instant::now();

                let updated = this.update(cx, |this, cx| {
                    this.load_worktrees(cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        self._worktree_watch_task = Some(task);
    }

    /// The prune candidate list: every worktree that is a prune candidate on its own merits
    /// ([`rail::is_prunable`]) **and** has no live agent running with its cwd inside it -
    /// see [`rail::prunable_worktree_paths`]'s docs for why that second condition matters.
    /// Shared by the footer's displayed count and [`Self::execute_prune`], so what's shown
    /// always matches what a click will do.
    pub(crate) fn prunable_worktree_paths(&self) -> Vec<PathBuf> {
        let worktree_paths: Vec<PathBuf> = self
            .worktrees
            .iter()
            .filter(|item| item.error.is_none())
            .map(|item| item.path.clone())
            .collect();
        let live_agent_cwds: HashSet<PathBuf> =
            self.agents.iter().map(|agent| agent.cwd.clone()).collect();
        rail::prunable_worktree_paths(&worktree_paths, &self.worktree_notes, &live_agent_cwds)
    }

    /// The footer `prune` button's click handler. Destructive, so this is deliberately a
    /// two-click confirmation: the first click only arms [`Self::prune_confirm_armed`] and
    /// changes the button's label, without touching the filesystem. Only a *second* click
    /// while already armed calls [`Self::execute_prune`] - worth the extra click since
    /// `wt_core::is_dirty` follows git's ignored-file semantics, so a "clean" worktree can
    /// still hold gitignored state a misclick would destroy.
    pub(crate) fn request_prune(&mut self, cx: &mut Context<Self>) {
        let candidates = self.prunable_worktree_paths();

        if candidates.is_empty() {
            self.prune_confirm_armed = false;
            self.discard_confirm_armed = None;
            self.prune_status = Some("nothing to prune".to_string());
            cx.notify();
            return;
        }

        if !self.prune_confirm_armed {
            self.prune_confirm_armed = true;
            self.prune_status = Some(rail::prune_confirm_label(candidates.len()));
            cx.notify();
            return;
        }

        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        self.execute_prune(candidates, cx);
    }

    /// Removes `candidates` via `wt_core::remove_worktree`. Only called once
    /// [`Self::request_prune`]'s confirmation step is satisfied, with paths
    /// [`Self::prunable_worktree_paths`] itself produced.
    pub(in crate::rail) fn execute_prune(
        &mut self,
        candidates: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.prune_in_flight {
            // Defense in depth alongside `Self::render_rail_footer`'s own gating of the prune
            // button while a batch is running.
            self.prune_status = Some("prune already running\u{2026}".to_string());
            cx.notify();
            return;
        }
        let repo_path = self.focused_repo_path();
        self.prune_in_flight = true;
        self.prune_status = Some(rail::pruning_label(candidates.len()));
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn({
                    let repo_path = repo_path.clone();
                    async move {
                        let mut removed = 0usize;
                        let mut errors = Vec::new();
                        for path in candidates {
                            match wt_core::remove_worktree(&repo_path, &path, false) {
                                Ok(()) => removed += 1,
                                Err(err) => errors.push(format!("{}: {err}", path.display())),
                            }
                        }
                        (removed, errors)
                    }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.prune_in_flight = false;
                let (removed, errors) = outcome;
                this.prune_status = Some(if errors.is_empty() {
                    rail::pruned_label(removed)
                } else {
                    format!(
                        "pruned {removed}; {} failed: {}",
                        errors.len(),
                        errors.join("; ")
                    )
                });
                this.load_worktrees(cx);
                cx.notify();
            });
        });
        self._prune_task = Some(task);
    }

    /// The whole left column (`design_handoff_jerry_ade/README.md`'s Zone 1): the sidebar strip,
    /// the filter row, the real scrollable body of whichever view the strip has selected, and the
    /// footer - see the README's "Rail chrome" section for the exact band heights this composes
    /// (`theme::band::{CHROME_HEADER,FILTER_ROW,SURFACE_FOOTER}`).
    pub(crate) fn render_rail(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // `Rc`, not a plain `Vec`: `Self::render_rail_list`'s own `gpui::list` render-item
        // closure captures this and is kept around by GPUI across frames, so it must be cheap to
        // clone - see that function's own docs.
        let groups: std::rc::Rc<Vec<RepoGroup>> = std::rc::Rc::new(self.build_repo_groups(cx));
        // A window with no worktree row anywhere is §1's First-run/Empty-day state: "with no
        // worktrees there are no views to offer". Read off `all_rows` rather than `rows` for the
        // same reason every count in `RepoGroup` is: a filter query that hides every row must not
        // make the strip's cells vanish.
        let has_worktrees = groups.iter().any(|group| !group.all_rows.is_empty());
        let view = self.effective_sidebar_view(has_worktrees);
        // Built once, for the same reason `groups` is: the Problems cell's marker and the
        // Problems body are two answers about one set of diagnostics, and `REVISION-2026-08-13.md`
        // §2's "tallied over their own data" is only structurally guaranteed if there is one
        // `data` for both to be over.
        let problems = self.worktree_problems();
        let cells = rail_strip::strip_view_cells(
            has_worktrees,
            view,
            agents_needing_you(&self.build_agent_rows(cx)),
            rail_strip::ProblemTally::over(&problems),
        );

        div()
            .id("agent-rail")
            // The app's real "nowhere else to put focus" fallback target - see
            // `AdeApp::rail_focus_handle`'s own docs for why the fallback lives on this
            // deliberately context-less root rather than on the filter row below it.
            .track_focus(&self.rail_focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_sidebar_strip(&cells, cx))
            .child(self.render_rail_filter_row(view, cx))
            .when_some(self.render_worktrees_error_banner(), |el, banner| {
                el.child(banner)
            })
            .when_some(
                self.render_worktree_selection_notice_banner(cx),
                |el, banner| el.child(banner),
            )
            // The scrolling body itself - see `Self::render_sidebar_body`'s own docs on why the
            // two views no longer share one scroll-owning wrapper here: the Worktrees view's own
            // `Self::render_rail_list` now owns real virtualized scrolling
            // ([`Self::rail_list_state`]) rather than eagerly building every row into a plain
            // `overflow_y_scroll()` div, while Problems keeps that plain scroller
            // ([`Self::rail_scroll_handle`]) - genuinely few rows, no virtualization needed.
            .child(self.render_sidebar_body(view, &groups, &problems, cx))
            .child(self.render_rail_footer(cx))
    }

    /// A visible error banner for [`Self::worktrees_error`] (`wt_core::list_worktrees_porcelain`
    /// failing outright, e.g. a corrupt repository) - shown as a standing banner rather than
    /// replacing the whole agent list, so already-open agents stay usable even when the
    /// worktree listing itself is broken.
    pub(in crate::rail) fn render_worktrees_error_banner(&self) -> Option<impl IntoElement> {
        let error = self.worktrees_error.as_ref()?;
        Some(
            div()
                .id("rail-worktrees-error")
                .flex_none()
                .px(px(10.0))
                .py(px(6.0))
                .bg(theme::status::FAIL_BG)
                .border_b_1()
                .border_color(theme::border::RAIL_INNER)
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(10.0))
                .text_color(theme::status::FAIL)
                .child(format!("failed to list worktrees: {error}")),
        )
    }

    /// GitHub issue #12's "the user is notified" selection-recovery banner - shown when
    /// [`Self::load_worktrees`] found the previously selected worktree gone (or newly broken)
    /// and fell [`Self::selected`] back to the main worktree
    /// ([`Self::worktree_selection_notice`]'s own docs). Amber (`theme::status::ASK`/`ASK_BG`),
    /// not the hard-failure red [`Self::render_worktrees_error_banner`] uses above - this is
    /// "something changed out from under you", not "the listing itself is broken". Click to
    /// dismiss, mirroring `crate::sidebar::render::AdeApp::render_file_tree`'s own
    /// `tree_op_error` banner.
    pub(in crate::rail) fn render_worktree_selection_notice_banner(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let notice = self.worktree_selection_notice.clone()?;
        Some(
            div()
                .id("rail-worktree-selection-notice")
                .flex_none()
                .w_full()
                .cursor_pointer()
                .px(px(10.0))
                .py(px(6.0))
                .bg(theme::status::ASK_BG)
                // GitHub issue #128: the tooltip already says "Click to dismiss," but nothing
                // visually confirmed a hover was even registered. No dedicated hover token for
                // this status-coloured bg exists, so this dims it slightly rather than inventing
                // a one-off theme constant - the same `.resolve().opacity(...)` technique
                // `crate::code_surface::minimap`'s scrollbar thumb hover already uses for an
                // analogous "still the same colour, just a distinguishable second state" need.
                .hover(|el| el.bg(theme::status::ASK_BG.resolve().opacity(0.7)))
                .border_b_1()
                .border_color(theme::border::RAIL_INNER)
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(10.0))
                .text_color(theme::status::ASK)
                .tooltip(text_tooltip("Click to dismiss"))
                .child(notice)
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.worktree_selection_notice = None;
                    cx.notify();
                })),
        )
    }

    /// Filter row 30: `/` plus the real typed query, or the placeholder text when empty -
    /// see [`Self::handle_filter_key_down`] for the (deliberately minimal) text input.
    pub(in crate::rail) fn render_rail_filter_row(
        &self,
        view: rail_strip::SidebarView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row = div()
            .id("rail-filter-row")
            .track_focus(&self.filter_focus_handle)
            // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why the tag and
            // the listener both live on this exact node.
            .key_context("text-input")
            .on_action(cx.listener(Self::handle_filter_text_undo))
            .on_action(cx.listener(Self::handle_filter_text_redo))
            .on_key_down(cx.listener(Self::handle_filter_key_down));
        // GitHub issue #336's four clipboard/select-all actions, on the same node and for the same
        // structural-routing reason the two undo actions above are.
        self.wire_text_input_actions(row, Self::filter_query_handle(), cx)
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.filter_focus_handle, cx);
            }))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            // `Jerry.dc.html:109` - `padding:0 12px`, not 10px.
            .px(px(12.0))
            .h(theme::band::FILTER_ROW)
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::GHOST)
                    .child("/"),
            )
            .child(self.render_simple_input_row(
                SimpleInput {
                    caret_selector: "rail-filter-caret".into(),
                    text_selector: "rail-filter-text".into(),
                    focus_handle: Some(&self.filter_focus_handle),
                    text: self.filter_query.as_str(),
                    caret_offset: self.filter_query.caret(),
                    selection: self.filter_query.selection(),
                    placeholder: match view {
                        rail_strip::SidebarView::Worktrees => "filter worktrees and agents",
                        rail_strip::SidebarView::Problems => "filter problems",
                        rail_strip::SidebarView::History => "filter runs",
                    },
                    font: theme::font::MONO,
                    text_size: self.ui_text_size(10.5),
                    text_color: theme::text::DIM,
                    placeholder_color: theme::text::GHOST,
                    caret: widgets::SimpleInputCaret::default(),
                    field: Some(Self::filter_query_handle()),
                },
                cx,
            ))
    }

    /// The rail filter's own field handle - what click/drag selection and the four clipboard
    /// actions act on, and the same "an edit disarms the pending confirmations" work
    /// [`Self::handle_filter_key_down`] does for an ordinary keystroke.
    fn filter_query_handle() -> widgets::TextFieldHandle {
        widgets::TextFieldHandle::new(|app: &mut AdeApp| Some(&mut app.filter_query)).on_changed(
            |app: &mut AdeApp, _cx| {
                app.prune_confirm_armed = false;
                app.discard_confirm_armed = None;
            },
        )
    }

    /// Builds this rail's repo groups fresh from live state every render (cheap: no I/O, just
    /// field reads plus the cached [`Self::diff_cache`]/[`Self::worktree_notes`] snapshots) -
    /// see [`Self::build_worktree_rows`]'s docs. The shared foundation for
    /// [`Self::render_rail_list`] and, since it returns plain data rather than GPUI elements,
    /// this module's own tests.
    pub(crate) fn build_repo_groups(&self, cx: &mut Context<Self>) -> Vec<RepoGroup> {
        let rows = self.build_worktree_rows(cx);
        let filtered: Vec<WorktreeRow> =
            rail::filter_worktree_rows(&rows, self.filter_query.as_str())
                .into_iter()
                .cloned()
                .collect();
        // Every open agent, in every repo - see this function's own docs above for why this,
        // not `&[]`, is what a non-focused repo's rows must be matched against too.
        let agent_rows = self.build_agent_rows(cx);

        let repo_inputs: Vec<RepoWorktrees> = self
            .repos
            .iter()
            .map(|repo| {
                let is_focused = Some(repo.id) == self.focused_repo;
                let (all_rows, rows_loaded) = if is_focused {
                    (rows.clone(), true)
                } else {
                    let entries = self.build_worktree_entries_from(&repo.worktrees);
                    (
                        rail::build_worktree_rows(&entries, &agent_rows),
                        repo.worktrees_loaded,
                    )
                };
                let rows = if is_focused {
                    filtered.clone()
                } else {
                    rail::filter_worktree_rows(&all_rows, self.filter_query.as_str())
                        .into_iter()
                        .cloned()
                        .collect()
                };
                RepoWorktrees {
                    repo_id: repo.id,
                    repo_name: repo.name.clone(),
                    all_rows,
                    rows,
                    rows_loaded,
                }
            })
            .collect();
        rail::group_worktrees_by_repo(repo_inputs)
    }

    /// The rail's one real structure (`design_handoff_jerry_ade/revision 3/
    /// REVISION-2026-07-31.md` §2.1: "Two levels, always: **repo group → worktree → agents**.
    /// There is **no rail mode toggle**"). See [`Self::build_repo_groups`] for how the groups
    /// themselves are built.
    pub(in crate::rail) fn render_rail_list(
        &self,
        groups: &std::rc::Rc<Vec<RepoGroup>>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // GitHub issue #113: a repo with zero open worktrees still renders its own group
        // (`Self::render_repo_group_header`/[`rail::flatten_rail_list_items`] emit every group's
        // header regardless of `rows`/`all_rows`), so the only case left with genuinely nothing
        // to show is no repo at all - defensive rather than reachable through any real UI path
        // today, since `Self::render_rail` (this function's only caller) is itself only ever
        // rendered once `Self::focused_repo` is `Some`, which requires at least one entry in
        // `Self::repos`.
        if groups.is_empty() {
            return self.render_rail_empty_message("no worktrees found");
        }

        let items: std::rc::Rc<Vec<rail::RailListItem>> =
            std::rc::Rc::new(rail::flatten_rail_list_items(groups, |row| {
                self.worktree_is_expanded(row)
            }));
        // `ListState` owns a measured height per item, so it has to be told when the item set
        // changes size - see `Self::render_changes_sections`'s own docs on this exact idiom
        // (`gpui::ListState::reset` takes `&self`, which is what lets this run from a `&self`
        // render). Reset only on a real change: a reset drops the scroll position, and doing it
        // on every render - including the ones a hover-triggered `Window::refresh()` causes, this
        // whole change's entire reason for existing - would pin the rail to the top on every
        // hover.
        if self.rail_list_state.item_count() != items.len() {
            self.rail_list_state.reset(items.len());
        }

        let build_items = items.clone();
        let build_groups = groups.clone();
        let list = gpui::list(
            self.rail_list_state.clone(),
            cx.processor(
                move |this: &mut Self,
                      index: usize,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
                    // Bounds-checked rather than indexed, mirroring `Self::render_changes_sections`'s
                    // own dispatch: this frame's flattened snapshot may be stale by the time
                    // `gpui::list` actually asks for one of its rows, and a stale index must
                    // render nothing rather than panic.
                    match build_items.get(index) {
                        Some(item) => this.render_rail_list_item(&build_groups, item, window, cx),
                        None => div().into_any_element(),
                    }
                },
            ),
        )
        .w_full()
        .flex_1()
        .min_h_0();

        // See `Self::render_file_tree`'s own docs (mirrored by `Self::render_changes_sections`)
        // on why the scrollbar must be a sibling of the list, inside its own non-scrolling
        // `.relative()` wrapper - the outer `#agent-rail-list` band is not that wrapper itself
        // any more (it no longer scrolls: `gpui::list` owns its own scroll offset via
        // [`Self::rail_list_state]`), just the same real painted band
        // `crate::rail::menu_render`'s own tests measure against.
        div()
            .id("agent-rail-list")
            .debug_selector(|| "agent-rail-list".to_string())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("rail-repo-groups")
                    // Lets a real test prove the Worktrees *body* really is what the strip's
                    // Worktrees cell switches to, and really is gone when Problems is selected -
                    // see `crate::rail::strip_render`'s own
                    // `clicking_a_cell_really_switches_the_panel_under_it`.
                    .debug_selector(|| "rail-repo-groups".to_string())
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .children(scrollbar::render_vertical_scrollbar(
                        "rail-scrollbar",
                        &self.rail_list_state,
                        &[],
                        cx,
                    )),
            )
            .into_any_element()
    }

    /// Dispatches one flattened [`rail::RailListItem`] to the renderer for its kind - see that
    /// type's own docs. `groups` and `item` are both resolved fresh against this frame's own
    /// snapshot by [`Self::render_rail_list`]'s caller (never a captured, possibly-stale
    /// reference), the same defensive re-resolve `crate::sidebar::render::AdeApp::
    /// render_section_row` already documents for the identical reason.
    fn render_rail_list_item(
        &self,
        groups: &[RepoGroup],
        item: &rail::RailListItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let trailing_pb = item.is_last_in_worktree_block(
            groups,
            match item {
                rail::RailListItem::WorktreeRow {
                    group_index,
                    row_index,
                } => groups
                    .get(*group_index)
                    .and_then(|group| group.rows.get(*row_index))
                    .is_some_and(|row| self.worktree_is_expanded(row)),
                _ => false,
            },
        );
        match item {
            rail::RailListItem::RepoHeader { group_index } => match groups.get(*group_index) {
                Some(group) => self
                    .render_repo_group_header(group, *group_index)
                    .into_any_element(),
                None => div().into_any_element(),
            },
            rail::RailListItem::RepoEmptyMessage { group_index } => {
                match groups.get(*group_index) {
                    Some(group) => self
                        .render_repo_group_empty_message(group)
                        .into_any_element(),
                    None => div().into_any_element(),
                }
            }
            rail::RailListItem::WorktreeRow {
                group_index,
                row_index,
            } => match groups
                .get(*group_index)
                .and_then(|group| group.rows.get(*row_index))
            {
                Some(row) => {
                    let is_expanded = self.worktree_is_expanded(row);
                    self.render_worktree_row(row, *row_index, is_expanded, trailing_pb, cx)
                        .into_any_element()
                }
                None => div().into_any_element(),
            },
            rail::RailListItem::AgentRow {
                group_index,
                row_index,
                agent_index,
            } => match groups
                .get(*group_index)
                .and_then(|group| group.rows.get(*row_index))
                .and_then(|row| row.agents.get(*agent_index))
            {
                Some(agent) => self
                    .render_agent_row(agent, trailing_pb, cx)
                    .into_any_element(),
                None => div().into_any_element(),
            },
            rail::RailListItem::EarlierRunsLink {
                group_index,
                row_index,
            } => match groups
                .get(*group_index)
                .and_then(|group| group.rows.get(*row_index))
            {
                Some(row) => self
                    .render_earlier_runs_link(&row.path, row.history.len(), trailing_pb, cx)
                    .into_any_element(),
                None => div().into_any_element(),
            },
        }
    }

    pub(in crate::rail) fn render_rail_empty_message(
        &self,
        message: &'static str,
    ) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .p(px(12.0))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::text::GHOST)
                    .child(message),
            )
            .into_any_element()
    }

    /// One repo group (§2.0-2.1): the header (name, `N wt` count, the two urgency dot+count pairs
    /// when non-zero, and a per-repo `+`), then either every worktree row already
    /// ranked most-urgent-first by [`rail::group_worktrees_by_repo`], or - GitHub issue #113 - a
    /// real inline message when this repo has none to show, rather than the header (and the repo
    /// itself) simply disappearing from the rail. Every repo's header renders regardless of
    /// `rows`/`all_rows`, but the header is deliberately **not clickable** - no `on_click`, no
    /// cursor/hover affordance. That is an explicit product decision, made after two subtler
    /// header-click behaviors were both rejected in review: in the rail, only worktree rows and
    /// agent rows are click targets, and only worktrees have tabs; a repo header is a plain
    /// group label. Switching to a different repo is done by clicking any worktree row under its
    /// group - [`crate::root::AdeApp::select_worktree_by_path`]'s cross-repo fallback runs the
    /// entire real repo switch ([`Self::checkout_repo_from_rail`]) itself, so the header never
    /// needs a click handler for repo switching to work.
    pub(in crate::rail) fn render_repo_group_header(
        &self,
        group: &RepoGroup,
        index: usize,
    ) -> impl IntoElement {
        let repo_id = group.repo_id;
        let is_first = index == 0;

        let header = div()
            .id(("repo-group-header", repo_id.0))
            .debug_selector(move || format!("repo-group-header-{}", repo_id.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(26.0))
            .px(px(12.0))
            // §4s/§4u: a bare rule above the label, and none at all on the first band.
            .when(!is_first, |el| {
                el.border_t_1().border_color(theme::border::DIVIDER)
            })
            // Deliberately no `on_click`, no `cursor_pointer`, no hover background: this header
            // is a plain label, not a control - see this function's own docs. Per this file's
            // established "non-actionable control drops cursor_pointer/hover/on_click" rule (the
            // comment near `render_worktree_row`'s `is_selected` handling), an unclickable row
            // must not carry click affordances either.
            .child(
                div()
                    // The one shrinkable thing in the row (§4).
                    .min_w_0()
                    .flex_shrink_1()
                    .truncate()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::rail::REPO_HEADER_NAME)
                    .child(group.repo_name.to_uppercase()),
            )
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::PATH)
                    .child(if group.rows_loaded {
                        format!("{} wt", group.all_rows.len())
                    } else {
                        // `group.all_rows` is empty here only because this repo's worktree data
                        // was never fetched (see `Self::build_repo_groups`'s docs) - rendering
                        // `0 wt` would falsely claim this repo really has no worktrees. An em
                        // dash is the honest "not loaded" signal instead.
                        "\u{2014} wt".to_string()
                    }),
            )
            .child(div().flex_1().min_w(px(4.0)))
            .children(self.render_repo_urgency_count(
                repo_id,
                UrgencyCount::NeedsInput,
                group.needs_input_count(),
            ))
            .children(self.render_repo_urgency_count(
                repo_id,
                UrgencyCount::Failed,
                group.failed_count(),
            ));

        // §4s/§4u: 3px to this header's own first worktree row (against the 7px between rows),
        // and a 12px spacer above every band but the first - both real sibling boxes now, not
        // `mb`/`pt` on the header itself. `gpui::list`/`gpui::UniformList` measure each item via
        // `Element::layout_as_root`, which - verified directly against a real render, the same way
        // `crate::root::scrollbar`'s own geometry notes verify against
        // `vendor/zed/crates/gpui/src/elements/uniform_list.rs` - does not fold a root element's
        // own margin into its measured height the way an ordinary flex sibling's would be, so a
        // flattened list item's inter-item spacing has to be real boxes in its own returned
        // element tree instead. The header's own 26px measured height (`rail_rev6_render_tests::
        // the_repo_header_sits_closer_to_its_rows_than_the_rows_sit_to_each_other`) stays exactly
        // that - unaffected by either spacer, both of which sit outside it.
        div()
            .w_full()
            .flex()
            .flex_col()
            .when(!is_first, |el| el.child(div().h(px(12.0))))
            .child(header)
            .child(div().h(px(3.0)))
    }

    /// The inline "not loaded yet" / "no worktrees open yet" / "no worktrees match this filter"
    /// message shown in place of a repo group's rows when it has none to show -
    /// `Self::render_rail_list`'s flattened [`rail::RailListItem::RepoEmptyMessage`] resolves
    /// here. Split out of the old `render_repo_group` for the same reason
    /// [`Self::render_repo_group_header`] was.
    pub(in crate::rail) fn render_repo_group_empty_message(
        &self,
        group: &RepoGroup,
    ) -> impl IntoElement {
        div()
            .w_full()
            .px(px(12.0))
            .pb(px(6.0))
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(9.5))
            .text_color(theme::text::GHOSTER)
            .child(if !group.rows_loaded {
                // No "click to open" here: the header is not clickable (see
                // `Self::render_repo_group_header`'s own docs), and this repo's own real
                // background fetch (`crate::root::AdeApp::start_repo_worktrees_polling`) resolves
                // this state on its own moments later.
                "not loaded yet"
            } else if group.all_rows.is_empty() {
                "no worktrees open yet"
            } else {
                "no worktrees match this filter"
            })
    }

    /// Whether `row`'s agent rows are currently shown - an explicit per-worktree override in
    /// [`Self::rail_collapse_overrides`] if the caret has ever been clicked for this path,
    /// otherwise the real default (§2.2: "Worktrees whose most urgent agent is idle start
    /// collapsed"). An agent-less row has no caret at all, so this is only ever consulted
    /// (via [`Self::render_worktree_row`]) when `row.agents` is non-empty.
    pub(in crate::rail) fn worktree_is_expanded(&self, row: &WorktreeRow) -> bool {
        match self.rail_collapse_overrides.get(&row.path) {
            Some(expanded) => *expanded,
            None => {
                self.current_worktree_path().as_deref() == Some(row.path.as_path())
                    || row.aggregate_status() != Status::Idle
            }
        }
    }

    /// The worktree row caret's click handler - flips whatever [`Self::worktree_is_expanded`]
    /// just reported for this path into an explicit, remembered override.
    pub(in crate::rail) fn toggle_worktree_collapsed(
        &mut self,
        worktree_path: PathBuf,
        currently_expanded: bool,
        cx: &mut Context<Self>,
    ) {
        self.rail_collapse_overrides
            .insert(worktree_path, !currently_expanded);
        cx.notify();
    }

    /// One worktree row's own header band (§2.2: 27 high, padding `0 10 0 6`, gap 6) - the rail's
    /// real "worktree owns N agents" structure now renders its agent rows (§2.3) and history rows
    /// as their own sibling [`rail::RailListItem`]s (see that type's own docs for why), so this
    /// builds only the one 27px band every worktree row always has, whether or not it is
    /// expanded. `index` (unique within its repo group) disambiguates element ids for the real
    /// degenerate case `crate::rail::worktrees::WorktreeItem`'s docs call out: more than one
    /// unreadable worktree entry shares the same (empty) `path`, which alone would collide.
    pub(in crate::rail) fn render_worktree_row(
        &self,
        row: &WorktreeRow,
        index: usize,
        is_expanded: bool,
        trailing_pb: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = format!("worktree-row-{index}-{}", row.path.display());

        if let Some(error) = &row.error {
            // A real error row, per `crate::rail::worktrees::WorktreeItem`'s documented intent:
            // visible, not silently dropped - and deliberately not clickable (an errored
            // entry has no usable, real path to select into).
            return div()
                .id(id)
                .w_full()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .px(px(10.0))
                .py(px(6.0))
                .child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(self.ui_text_size(12.0))
                        .text_color(theme::status::FAIL)
                        .child(row.label.clone()),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(10.0))
                        .text_color(theme::status::FAIL)
                        .child(error.clone()),
                )
                .into_any_element();
        }

        // The rail's own "this row is the selected worktree" state, read from the exact same
        // single source of truth the tab strip scopes itself to - so the two can never disagree.
        // With `Self::current_worktree_path`'s repo-root fallback removed, "nothing is selected" now
        // genuinely draws *no* row as selected, rather than lighting up whichever row happened to
        // sit at the repo root while the tab strip showed something else entirely.
        let is_selected = self.current_worktree_path().as_deref() == Some(row.path.as_path());
        let has_agents = !row.agents.is_empty();
        // GitHub issue #227: history is no longer a *child* of this row. It moved out of the rail
        // into the sidebar's own History view, and what is left here is the `↺ N earlier runs`
        // line under the row ([`Self::render_earlier_runs_link`]) - which is not behind the caret,
        // so the caret is back to meaning exactly "this worktree has live agents".
        let has_children = has_agents;
        // `is_expanded` is a real parameter now, not recomputed here - see this function's own
        // docs on why. Guard against a childless row somehow being passed `is_expanded: true`
        // anyway (defensive; `rail::flatten_rail_list_items` never does), the same way the old
        // local computation always `&&`-ed against `has_children`.
        let is_expanded = has_children && is_expanded;

        let edge_color =
            worktree_row_edge(row.aggregate_status(), row.note.is_prunable(), is_selected);

        // `#dde2e7` active / `#c2c7cc` with agents / `#8b9197` bare (§2.2).
        let branch_color: gpui::Rgba = if is_selected {
            theme::text::SELECTED.into()
        } else if has_agents {
            theme::text::STRONG.into()
        } else {
            theme::text::DIM.into()
        };

        // The app's **one** disclosure caret (§4o/§4p): 10px `#8b9197` in a 13-wide box the full
        // height of the row, "so the whole left column is clickable", with a hover lift and a
        // tooltip. It was 8px `#6b7178` in an 11px box - "the smallest interactive target in the
        // window and the one you hit most while triaging". §4p draws the line this sits on: a
        // *disclosure* caret (rail rows, panel sections, group headers) gets this treatment, while
        // a *dropdown chevron* bound to a button or chip stays at 8-8.5px.
        //
        // The slot itself is always emitted, even for a childless worktree - it stays empty
        // (no glyph, no tooltip, no hover, no click) rather than disappearing, so every row's
        // branch label lands at the same x offset regardless of whether that row has anything to
        // expand (design_handoff_jerry_ade revision 5's own `w.caret` binding does the same: the
        // glyph is emptied to "" but its fixed-width wrapper div never leaves the layout).
        let caret = {
            let worktree_path = row.path.clone();
            div()
                .id(("worktree-caret", index as u64))
                .debug_selector(move || format!("worktree-caret-{index}"))
                .flex_none()
                .w(px(13.0))
                .h(px(27.0))
                .flex()
                .items_center()
                .justify_center()
                .font(font(theme::font::MONO))
                .text_size(self.ui_text_size(10.0))
                .text_color(theme::text::DIM)
                .when(has_children, |el| {
                    el.cursor_pointer()
                        // GitHub issue #128 - same lightweight text-only hover
                        // `Self::render_status_zoom_value` uses for an equally small, box-free
                        // clickable glyph. It works because the shared caret helper below paints
                        // no colour of its own - the glyph inherits this wrapper's, which is the
                        // element the hover is armed on.
                        .hover(|el| el.text_color(theme::text::STRONG))
                        // §4's "tooltips on every icon-only control". The wording is
                        // `Jerry.dc.html`'s own, which states the thing the glyph cannot: this
                        // control toggles the group *without* selecting the worktree, which is
                        // what the `stop_propagation` below actually does.
                        .tooltip(text_tooltip("Collapse or expand without selecting"))
                        // `STAGE-A-CHANGELOG.md` §4o/§4p: this used to be its own hand-drawn
                        // glyph, which is exactly the drift §4p closed - "every disclosure caret
                        // is one control". It is now the shared one
                        // (`crate::root::widgets::render_disclosure_caret`), the same call the
                        // Changes panel's four section headers make, so the two cannot diverge
                        // again. The 13x27 hit box, the tooltip and the hover stay here: they are
                        // this row's business, not the glyph's.
                        .child(render_disclosure_caret(
                            is_expanded,
                            self.ui_text_size(10.0),
                        ))
                        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.toggle_worktree_collapsed(worktree_path.clone(), is_expanded, cx);
                        }))
                })
        };

        let branch_div = div()
            .min_w_0()
            .flex_shrink_1()
            .truncate()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(self.ui_text_size(11.0))
            .text_color(branch_color)
            .child(row.branch.clone().unwrap_or_else(|| row.label.clone()));

        let mut trailing = div().flex().flex_none().items_center().gap(px(4.0));
        if has_agents {
            if !is_expanded {
                // The collapsed row's per-agent status dots - along with the full agent rows when
                // expanded, these are what carry status now that the row's own left edge no
                // longer does (§4m). One dot per agent, most urgent first: the honest, per-agent
                // version of the lossy `max()` the edge used to state.
                let mut dot_statuses: Vec<Status> =
                    row.agents.iter().map(|agent| agent.status).collect();
                dot_statuses.sort_by_key(|status| status.urgency_rank());
                trailing = trailing.child(div().flex().items_center().gap(px(3.0)).children(
                    dot_statuses.into_iter().map(|status| {
                        div()
                            .w(px(4.0))
                            .h(px(4.0))
                            .rounded_full()
                            .bg(status.color())
                    }),
                ));
            }
            // §4o: the rail's diffstat is coloured like every other diffstat in the app, which is
            // only possible because `rail::diff_stat_parts` returns its parts rather than one
            // pre-joined string. The prose fallbacks below (`checkout · clean`, `merged ·
            // prunable`) are a different kind of value and stay neutral.
            if let Some((add, del)) = row.diff_stat_parts() {
                trailing = trailing.child(
                    div()
                        .flex_none()
                        .whitespace_nowrap()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::diff::STAT_ADD)
                        .child(add),
                );
                if let Some(del) = del {
                    trailing = trailing.child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::diff::STAT_DEL)
                            .child(del),
                    );
                }
            }
        }

        let path = row.path.clone();
        let header = div()
            .id(id.clone())
            // Test-only bounds lookup, the same real `gpui::VisualTestContext::debug_bounds`
            // hook this file's `repo-group-header-N` already carries - so a
            // test can simulate a real mouse click at this row's painted position rather than
            // reaching past the render side and calling its handler directly. Added for the
            // cross-repo worktree click (`repo_checkout_tests::
            // clicking_a_non_focused_repos_worktree_row_switches_repo_and_selects_it`), whose
            // whole point is that the row is genuinely rendered and genuinely clickable for a
            // repo that isn't focused.
            .debug_selector(move || id)
            .cursor_pointer()
            .w_full()
            .flex()
            .items_center()
            .h(px(27.0))
            .pl(px(6.0))
            .pr(px(10.0))
            .gap(px(6.0))
            // The 2px gutter is always reserved (§4m: "The 2px gutter stays for alignment"); it
            // is only *painted* when this row is the selected one. A `None` border colour paints
            // nothing at all, which is the off state of a one-meaning channel.
            .border_l(px(2.0))
            .when_some(edge_color, |el, token| el.border_color(token))
            .when(is_selected, |el| el.bg(theme::rail::WORKTREE_ACTIVE_BG))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(theme::rail::WORKTREE_HOVER_BG))
            })
            .on_click(cx.listener({
                let path = path.clone();
                move |this, _event: &ClickEvent, window, cx| {
                    this.select_worktree_by_path(&path, window, cx);
                }
            }))
            // The worktree row's context menu (GitHub issue #290) - anchored to the pointer, not
            // to the row (`STAGE-A-CHANGELOG.md` §4u: "Rows are 27px and the pointer is what the
            // user aimed with"), and painted at the root, outside this scroller (§4).
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener({
                    let path = path.clone();
                    move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.open_rail_row_menu(
                            crate::rail::menu::RailMenuTarget::Worktree(path.clone()),
                            f32::from(event.position.x),
                            f32::from(event.position.y),
                            window,
                            cx,
                        );
                    }
                }),
            )
            .child(caret)
            .child(branch_div)
            .when(!has_agents, |el| {
                // `debug_selector`ed (GitHub issue #<earlier-runs-spacing>): the same
                // "`main checkout \u{b7} clean`" note the earlier-runs-link row's own regression
                // test measures against as a known-good, real single-line sibling row.
                let note_selector: &'static str =
                    Box::leak(format!("worktree-row-note-{index}").into_boxed_str());
                el.child(
                    div()
                        .debug_selector(move || note_selector.to_string())
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::text::GHOSTER)
                        .child(format!("\u{b7} {}", row.note.label())),
                )
            })
            .child(div().flex_1().min_w(px(2.0)))
            .child(trailing);

        // GitHub issue #12's "locked worktrees are visually marked, with the lock reason
        // surfaced (tooltip is fine)" - `row.note.is_locked` alone (already threaded through
        // `build_worktree_entries` from `WorktreeItem::is_locked`) is what drives the `·
        // locked`/`locked` text `WorktreeNote::label` already renders in the stat column above;
        // this adds the *reason* as a tooltip. Looked up from `self.worktrees` by path rather
        // than threaded onto `WorktreeRow` itself - `WorktreeNote` is shared with the periodic
        // status-poll snapshot (`crate::rail::state::compute_status_snapshot`) and already has a
        // lot of call sites; a worktree list is always small, so a linear lookup here per row per
        // render is real but negligible cost next to everything else this function already
        // computes.
        //
        // Scoped to this header band alone, not the whole worktree block the way it was before
        // this row's agent/history rows became their own sibling list items rather than this
        // row's own children (`rail::RailListItem`'s own docs): a locked worktree's own row is
        // what carries the mark, and its lock state has no separate meaning to state on a live
        // agent row underneath it.
        let header = if row.note.is_locked {
            let lock_reason = self
                .worktrees
                .iter()
                .find(|item| item.path == row.path)
                .and_then(|item| item.lock_reason.clone());
            let tooltip_text = match lock_reason {
                Some(reason) => format!("Locked: {reason}"),
                None => "Locked".to_string(),
            };
            header
                .tooltip(text_tooltip(tooltip_text))
                .into_any_element()
        } else {
            header.into_any_element()
        };

        // No question-preview card renders here, deliberately. `design_handoff_jerry_ade/revision
        // 3/REVISION-2026-07-31.md` §2.3, verbatim: "**No question preview.** The amber ask box is
        // gone from the rail; the question belongs in the agent pane where it can be answered."
        // This is a design-driven removal of a card that really did ship (GitHub issue #268),
        // confirmed as deliberate on 2026-08-14, not a regression: an amber box quoting a question
        // in a surface with no way to answer it costs the rail's densest column two lines per
        // asking worktree while the pane one click away already shows the question in full, live.
        // The row's `needs input` dot and state word are what the rail is for. `AgentRow` carries
        // no preview field at all any more (and `Self::build_agent_rows` no longer scrapes the pty
        // grid for one) - see `rail_correction_tests`.
        //
        // `trailing_pb`'s 7px is a real sibling spacer box, not `.pb()` on `header` itself: `header`
        // carries a fixed `.h(px(27.0))` (`taffy`'s default `BoxSizing::BorderBox`, which this
        // whole crate relies on - see e.g. `crate::root::scrollbar`'s own geometry notes), so
        // padding there would shrink the row's own 27px content area rather than add space below
        // it. See `Self::render_repo_group_header`'s own docs on the identical spacer idiom, used
        // there for the same "a flattened list item's own inter-item spacing has to be a real box,
        // not a style meant for an ordinary flex sibling" reason.
        if trailing_pb {
            div()
                .w_full()
                .flex()
                .flex_col()
                .child(header)
                .child(div().h(px(7.0)))
                .into_any_element()
        } else {
            header
        }
    }

    /// One agent row (§2.3): indented 13, a 1px spine (2px and status-coloured when this is the
    /// globally active agent - `Self::agents::active_id`), exactly two lines - chip/title/
    /// elapsed, then status dot/state word/trailing text/model. Clicking it selects this
    /// agent's tab *and* its worktree (`Self::select_agent` - already does both: it's the
    /// same real entry point the palette/tab-strip use to jump straight to one agent).
    pub(in crate::rail) fn render_agent_row(
        &self,
        agent: &AgentRow,
        trailing_pb: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // See `crate::work_surface::render::AdeApp::active_agent_pane_id`'s own docs: reading
        // `Agents::active_id` straight here would leave this row drawn as selected even while
        // the centre pane is genuinely showing the review/run/graph tab instead - the live user
        // report ("agent rows and history rows weren't unselected when switching between them").
        let is_selected = self.active_agent_pane_id() == Some(agent.id);
        let status = agent.status;
        let chip_icon = self.render_agent_chip_icon(agent.kind, px(15.0), self.ui_text_size(9.0));
        let state_color: gpui::Rgba = match status {
            Status::Ask | Status::Fail => status.color(),
            _ => theme::text::FAINT.into(),
        };
        let trailing_text = agent_trailing_text(agent);
        let trailing_color: gpui::Rgba = if status == Status::Fail {
            theme::button::DANGER_FG.into()
        } else {
            theme::text::FAINT.into()
        };
        let id = agent.id;

        div()
            .id(("agent-row", id))
            // Lets a real test click this exact row at its real painted position
            // (`gpui::VisualTestContext::debug_bounds`), the same hook the worktree row above it
            // carries - used both by the agent row's own context menu (GitHub issue #290), which
            // has no other honest way of being driven from a test, and by the indent geometry
            // test below. `crate::rail::menu_render` builds the same string for its own
            // anchoring, so the two must stay one format.
            .debug_selector(move || format!("agent-row-{id}"))
            .cursor_pointer()
            .w_full()
            .flex()
            .pl(px(13.0))
            // The row's real indent under its worktree: 13px of empty space (padding, not a
            // sized column - the connector doesn't sit centered *within* the 13px, it comes
            // after it), carrying the 1px `#1e2225` connector line, then the agent's own content
            // box - never padding *on* that content box, which would draw its border-left (the
            // status edge) flush with the worktree row's own left edge instead of indented under
            // it. `Jerry.dc.html`'s own agent row is exactly this shape (an outer
            // `padding-left:13px` flex wrapper holding the connector `div`, then the content
            // `div` with its own `border-left`) - GPUI's border draws at a box's outer edge same
            // as CSS, so folding padding-left and border-left onto one div here reproduced the
            // bug the opposite way.
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_agent(id, window, cx);
            }))
            // The agent row's context menu (GitHub issue #290). It sits on this outer wrapper -
            // the same element `Jerry.dc.html` hangs its own `onContextMenu="{{ a.ctx }}"` on -
            // so the whole row, indent and connector included, is a right-click target rather
            // than just the content box. `stop_propagation` keeps the event from also reaching an
            // ancestor's handler and replacing this menu with a worktree one - the same guard the
            // file tree's nested rows use.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.open_rail_row_menu(
                        crate::rail::menu::RailMenuTarget::Agent(id),
                        f32::from(event.position.x),
                        f32::from(event.position.y),
                        window,
                        cx,
                    );
                }),
            )
            .child(div().flex_none().w(px(1.0)).bg(theme::border::ZONE))
            .child(
                div()
                    .debug_selector(move || format!("agent-row-content-{id}"))
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pl(px(7.0))
                    .pr(px(10.0))
                    // §4n's tighter agent block: `5 10 6 7` -> `4 10 5 7`.
                    .pt(px(4.0))
                    .pb(px(5.0))
                    .gap(px(2.0))
                    // `Jerry.dc.html`'s own agent row: `border-left:2px solid {{ a.edge }}`, with
                    // `edge: live ? st.color : 'transparent'` - a *fixed* 2px slot painted only
                    // for the focused agent. Two things were wrong with the width/colour pair
                    // this replaces (`2px status` selected, `1px #1e2225` otherwise), both of
                    // them created by moving the indent onto the wrapper above: the 1px fallback
                    // is the *same* `#1e2225` as the connector `div` immediately to its left, so
                    // an unselected row drew the connector twice as thick as the design's own
                    // 1px; and the width flipping 1px -> 2px on selection shifted this row's
                    // whole content box sideways by a pixel the moment you clicked it. The 2px
                    // gutter is now always reserved and simply left unpainted when this agent
                    // isn't the focused one - the same "a channel with one meaning has exactly
                    // two states, on and off" the worktree row's own edge follows
                    // (`worktree_row_edge`).
                    .border_l(px(2.0))
                    .when(is_selected, |el| el.border_color(status.color()))
                    .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED))
                    .when(!is_selected, |el| {
                        el.hover(|el| el.bg(theme::rail::WORKTREE_HOVER_BG))
                    })
                    .child(
                        // Line 1: chip · task title · elapsed.
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(chip_icon)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font(font(theme::font::SANS))
                                    .text_size(self.ui_text_size(11.0))
                                    .text_color(agent_title_color(status, is_selected))
                                    .child(agent.title.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    // §4k: always the neutral time token, whatever the status.
                                    .font(font(theme::font::MONO))
                                    .text_size(self.ui_text_size(9.5))
                                    .text_color(theme::text::GHOST)
                                    .child(rail::format_elapsed(agent.elapsed)),
                            ),
                    )
                    .child(
                        // Line 2, indented 21 to the text column (chip width 15 + gap 6): status
                        // dot · state word · trailing text · model.
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .pl(px(21.0))
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(4.0))
                                    .h(px(4.0))
                                    .rounded_full()
                                    .bg(status.color()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font(font(theme::font::SANS))
                                    .text_size(self.ui_text_size(9.5))
                                    .text_color(state_color)
                                    .child(agent_state_word(status)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font(font(theme::font::SANS))
                                    .text_size(self.ui_text_size(9.5))
                                    .text_color(trailing_color)
                                    .child(trailing_text),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .font(font(theme::font::MONO))
                                    .text_size(self.ui_text_size(9.5))
                                    .text_color(theme::text::PATH)
                                    .child(agent.kind.label()),
                            ),
                    ),
            )
            .when(trailing_pb, |el| el.pb(px(7.0)))
    }

    /// A worktree row's `\u{21ba} N earlier runs` line (GitHub issue #227).
    fn render_earlier_runs_link(
        &self,
        path: &std::path::Path,
        count: usize,
        trailing_pb: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let target = path.to_path_buf();
        let element_id = gpui::SharedString::from(format!("earlier-runs-{}", path.display()));
        let selector = element_id.clone();
        let icon_selector: &'static str =
            Box::leak(format!("earlier-runs-icon-{}", path.display()).into_boxed_str());
        let label_selector: &'static str =
            Box::leak(format!("earlier-runs-label-{}", path.display()).into_boxed_str());
        let row = div()
            .id(element_id)
            .debug_selector(move || selector.to_string())
            .w_full()
            .flex()
            .items_center()
            .gap(px(5.0))
            .h(px(19.0))
            // Lands under the branch label, past the caret slot and the connector - the same x
            // every agent row's own content starts at.
            .pl(px(21.0))
            .pr(px(10.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .tooltip(text_tooltip("Open History for this worktree"))
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.open_history_for_worktree(target.clone(), window, cx);
            }))
            .child(
                div()
                    .debug_selector(move || icon_selector.to_string())
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOSTER)
                    .child("\u{21ba}"),
            )
            .child(
                div()
                    .debug_selector(move || label_selector.to_string())
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::FAINT)
                    .child(crate::run_history::model::earlier_runs_label(count)),
            );

        // `trailing_pb`'s 7px must be a real sibling spacer box, never `.pb()` on `row` itself:
        // `row` carries a fixed `.h(px(19.0))`, and `taffy`'s default `BoxSizing::BorderBox` (see
        // `Self::render_worktree_row`'s identical note on `header`, and
        // `Self::render_repo_group_header`'s own use of the same idiom) means padding added to a
        // fixed-height box eats into that box's own content area rather than adding space below
        // it. That is exactly what broke here: with `.pb(px(7.0))` on `row`, its 19px content area
        // shrank to 12px, and `items_center()` centered the icon and label glyphs in *that*
        // 12px - real measured bounds showed both landing 3.5px above the row's own vertical
        // center, spilling 1.5px above the row's own top edge (screenshot-reported "earlier run
        // row spacing is wrong"). A sibling spacer box leaves `row`'s own 19px untouched and adds
        // the 7px gap after it instead.
        if trailing_pb {
            div()
                .w_full()
                .flex()
                .flex_col()
                .child(row)
                .child(div().h(px(7.0)))
                .into_any_element()
        } else {
            row.into_any_element()
        }
    }

    /// The real `Y GB` (`+` suffixed if [`Self::disk_usage`] was truncated) disk-usage label, or
    /// `...` while the background scan hasn't reported a real total yet - shared by
    /// [`Self::render_rail_footer`] and the status bar's worktrees cluster
    /// (`status_bar::render::render_status_worktrees_cluster`), so the two can never format the
    /// same real aggregate differently.
    pub(crate) fn disk_usage_label(&self) -> String {
        match self.disk_usage {
            Some((bytes, truncated)) => {
                let label = rail::format_bytes(bytes);
                if truncated {
                    format!("{label}+")
                } else {
                    label
                }
            }
            None => "...".to_string(),
        }
    }

    /// The total real disk these prune candidates would free, or `None` while the background
    /// scan ([`Self::load_disk_usage`], whose per-worktree half is [`Self::worktree_disk_usage`])
    /// has not yet measured **every** one of them.
    pub(in crate::rail) fn prunable_disk_usage(&self, paths: &[PathBuf]) -> Option<(u64, bool)> {
        let mut total = 0u64;
        let mut truncated = false;
        for path in paths {
            let (bytes, was_truncated) = self.worktree_disk_usage.get(path).copied()?;
            total = total.saturating_add(bytes);
            truncated |= was_truncated;
        }
        Some((total, truncated))
    }

    /// Footer 28: real aggregate stats (`N worktrees · disk usage`) plus the real prune action.
    pub(in crate::rail) fn render_rail_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Includes error'd entries - the count should match what `wt_core::list_worktrees`
        // reported, problems included, not silently shrink.
        let worktree_count = self.worktrees.len();
        let disk_label = self.disk_usage_label();
        let prunable_paths = self.prunable_worktree_paths();
        let prunable_count = prunable_paths.len();

        // Mirrors `Self::render_merge_flow_footer`'s `in_flight` gating: while a prune batch is
        // running - and, equally, when there is nothing to prune (§7 rule 2: "A control that acts
        // on results does not exist when there are none") - this control drops
        // `cursor_pointer`/hover/`on_click` entirely rather than staying enabled-looking and
        // inviting a click `Self::execute_prune`'s guard would silently swallow.
        let enabled = !self.prune_in_flight && prunable_count > 0;
        let tooltip_text = if self.prune_in_flight {
            rail::pruning_label(prunable_count)
        } else if self.prune_confirm_armed && prunable_count > 0 {
            rail::prune_armed_tooltip(prunable_count)
        } else {
            rail::prune_tooltip(prunable_count, self.prunable_disk_usage(&prunable_paths))
        };
        let armed = enabled && self.prune_confirm_armed;
        let icon_color = if !enabled {
            theme::text::DISABLED
        } else if armed {
            theme::button::DANGER_FG
        } else {
            theme::text::FAINT
        };

        let prune_button = div()
            .id("rail-prune")
            .debug_selector(|| "rail-prune".to_string())
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(theme::band::ICON_BUTTON_HIT)
            .h(theme::band::ICON_BUTTON_HIT)
            .rounded(theme::radius::CHIP)
            .tooltip(text_tooltip(tooltip_text))
            .child(
                crate::icons::IconRow::new(
                    &self.settings.icon_pack,
                    crate::icons::IconSize::Control,
                )
                .draw(crate::icons::Icon::Trash, icon_color),
            );
        let prune_button = if enabled {
            prune_button
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.request_prune(cx);
                }))
        } else {
            prune_button.cursor_default()
        };

        div()
            .id("rail-footer")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .h(theme::band::SURFACE_FOOTER)
            .border_t_1()
            .border_color(theme::border::RAIL_INNER)
            .child({
                // `Self::worktree_history_status` deliberately does *not* share this slot - an
                // audit found `prune_status` (never cleared once set - see that field's own
                // docs) permanently masked every future worktree-history status after a single
                // prune click, including honest refusal messages that are the only pointer to
                // real recoverable content (e.g. `Error::DiscardRemovalFailedAfterStash`'s stash
                // id). It's shown instead in the status bar
                // (`Self::render_status_worktree_history_notice`), which - unlike this rail
                // footer - stays on screen even while Settings covers the whole workspace body.
                let status = self
                    .prune_status
                    .clone()
                    .unwrap_or_else(|| rail::worktree_disk_label(worktree_count, &disk_label));
                div()
                    .id("rail-footer-status")
                    .min_w_0()
                    .max_w(px(320.0))
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::GHOST)
                    .tooltip(text_tooltip(status.clone()))
                    .child(status)
            })
            .child(prune_button)
    }
}

/// Regression coverage for [`AdeApp::prune_in_flight`] - mirrors
/// `merge::flow::merge_regression_tests`'s real-git-repo, deterministic-executor idiom,
/// applied to the same bug class for pruning: arm, execute, arm again, execute again, with
/// all four `Self::request_prune` calls landing before the first batch's
/// `wt_core::remove_worktree` has run - must leave exactly one batch in flight, never two
/// racing ones sharing `Self::_prune_task`.
/// The rail agent row's review-ready file count (§2.3's `12 files` trailing text), which the
/// design calls out as needing "singular and plural both, everywhere" (GitHub issue #281).
#[cfg(test)]
mod agent_trailing_text_count_tests {
    use super::*;
    use std::time::Duration;

    fn review_row(review_file_count: Option<usize>) -> AgentRow {
        AgentRow {
            id: 1,
            kind: ProcessKind::claude(),
            title: "agent-a".to_string(),
            cwd: std::path::PathBuf::from("/a"),
            status: Status::Review,
            branch: Some("feature-x".to_string()),
            add: 0,
            del: 0,
            exit_code: None,
            activity: None,
            elapsed: Duration::ZERO,
            review_file_count,
        }
    }

    #[test]
    fn review_file_count_conjugates_at_zero_one_and_two() {
        assert_eq!(agent_trailing_text(&review_row(Some(0))), "0 files");
        assert_eq!(agent_trailing_text(&review_row(Some(1))), "1 file");
        assert_eq!(agent_trailing_text(&review_row(Some(2))), "2 files");
        assert_eq!(agent_trailing_text(&review_row(Some(12))), "12 files");
    }

    #[test]
    fn an_unmeasured_review_row_has_no_trailing_text() {
        assert_eq!(agent_trailing_text(&review_row(None)), "");
    }
}

#[cfg(test)]
mod prune_regression_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
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

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    /// Same linked-worktree idiom `merge::flow`'s test module uses. Created with no new
    /// commits, so its branch tip trivially equals `main`'s - a genuinely-merged, clean
    /// worktree without needing a second real merge to produce one.
    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        path
    }

    /// Wires up `app.worktrees`/`app.worktree_notes` directly with one prunable worktree,
    /// bypassing the periodic status-poll computation - `Self::prunable_worktree_paths` only
    /// reads these two fields plus `self.agents`, so this exercises the same code
    /// `Self::request_prune`/`Self::execute_prune` run in production.
    fn seed_one_prunable_worktree(app: &mut AdeApp, path: PathBuf, branch: &str) {
        app.worktrees.push(WorktreeItem {
            path: path.clone(),
            label: branch.to_string(),
            branch: Some(branch.to_string()),
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
        app.worktree_notes.insert(
            path,
            WorktreeNote {
                is_main: false,
                clean: Some(true),
                merge: Some(wt_core::diff::WorktreeMergeStatus {
                    base_branch: "main".to_string(),
                    merged: true,
                    head_committer_unix_seconds: None,
                }),
                is_locked: false,
            },
        );
    }

    #[gpui::test]
    fn a_second_confirm_while_first_batch_is_in_flight_does_not_prune_a_worktree_seeded_after_it(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let first = add_worktree(repo.path(), "first-feature", "first-feature-wt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            seed_one_prunable_worktree(app, first.clone(), "first-feature");
        });

        app.update(cx, |app, cx| app.request_prune(cx));
        assert!(app.read_with(cx, |app, _| app.prune_confirm_armed));

        // Click 2: confirm - spawns the real first prune batch, whose candidate list is
        // captured as exactly `[first]` right now. `prune_in_flight` is set synchronously,
        // before the background executor has run at all - the batch's own
        // `wt_core::remove_worktree` has not executed yet.
        app.update(cx, |app, cx| app.request_prune(cx));
        assert!(
            app.read_with(cx, |app, _| app.prune_in_flight),
            "prune_in_flight should be set synchronously by execute_prune"
        );
        assert!(!app.read_with(cx, |app, _| app.prune_confirm_armed));
        assert!(
            first.exists(),
            "the first batch's real background work must not have run yet - nothing has \
             parked the executor since it was spawned"
        );

        // Seed a second, independent prunable worktree *now*, while the first batch is still
        // genuinely in flight and before any executor progress has happened. The first
        // batch's candidate list was already captured above and cannot include this path.
        let second = add_worktree(repo.path(), "second-feature", "second-feature-wt");
        app.update(cx, |app, _cx| {
            seed_one_prunable_worktree(app, second.clone(), "second-feature");
        });

        app.update(cx, |app, cx| app.request_prune(cx));
        assert!(app.read_with(cx, |app, _| app.prune_confirm_armed));

        // Click 4: confirm again, while the first batch is still genuinely in flight. If the
        // guard works, `execute_prune` returns having done nothing - no second batch, no
        // second candidate list, `second` is never touched.
        app.update(cx, |app, cx| app.request_prune(cx));

        cx.run_until_parked();

        assert!(
            !app.read_with(cx, |app, _| app.prune_in_flight),
            "prune_in_flight must not be stranded at true - the real batch's own completion \
             handler must still run to reset it"
        );
        assert!(
            !first.exists(),
            "the first, genuinely in-flight batch must still have completed for real"
        );
        assert!(
            second.exists(),
            "a worktree seeded after the first batch was already in flight must survive a \
             second confirm click made before the first batch settled - if this fails, \
             `Self::prune_in_flight` did not actually prevent a second prune batch from \
             spawning and racing the first"
        );
    }
}

/// Real, `Context<AdeApp>`-driven coverage for Revision R12's rail rewrite: the repo-group →
/// worktree-row → agent-row structure (`design_handoff_jerry_ade/revision 3/
/// REVISION-2026-07-31.md` §2), the per-worktree collapse memory, and the agent row's "select
/// the worktree and raise this agent's tab" click behaviour.
#[cfg(test)]
mod rail_row_tests {
    use super::*;
    use crate::hooks::store::LiveRun;
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn worktree_item(path: PathBuf, label: &str) -> WorktreeItem {
        WorktreeItem {
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
    fn worktree_is_expanded_defaults_to_the_real_idle_rooted_rule(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let other_wt = tempfile::tempdir().expect("tempdir other wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(wt.path().to_path_buf(), "wt"),
                worktree_item(other_wt.path().to_path_buf(), "other-wt"),
            ];
        });
        app.update_in(cx, |app, window, cx| {
            // Select `other_wt`, not `wt` - `wt` (whose row this test inspects) must be the
            // real not-currently-selected case, or the new selected-worktree exemption would
            // make this test vacuous.
            app.select_worktree(1, window, cx);
        });

        let empty_row = app.read_with(cx, |app, cx| {
            app.build_worktree_rows(cx)
                .into_iter()
                .find(|row| row.path == wt.path())
                .expect("the seeded worktree must produce a row")
        });
        let running_agent = rail::AgentRow {
            id: 1,
            kind: ProcessKind::claude(),
            title: "wt".to_string(),
            cwd: wt.path().to_path_buf(),
            status: Status::Run,
            branch: Some("wt".to_string()),
            add: 0,
            del: 0,
            exit_code: None,
            activity: None,
            elapsed: std::time::Duration::from_secs(1),
            review_file_count: None,
        };
        let running_row = rail::WorktreeRow {
            agents: vec![running_agent],
            ..empty_row
        };
        assert_eq!(
            running_row.aggregate_status(),
            Status::Run,
            "sanity check: a running agent's row aggregates to Run"
        );
        assert!(
            app.read_with(cx, |app, _| app.worktree_is_expanded(&running_row)),
            "a worktree whose most urgent agent is running must default to expanded"
        );

        // Force the same row into Idle without waiting on a real clock: an agent-less
        // `WorktreeRow` (same path, no agents) aggregates to `Status::Idle` exactly the way a
        // real shell does once it goes quiet past `status::RUN_RECENT_OUTPUT_WINDOW` - the same
        // `aggregate_status` code path `Self::worktree_is_expanded` itself reads.
        let idle_row = rail::WorktreeRow {
            agents: Vec::new(),
            ..running_row
        };
        assert_eq!(idle_row.aggregate_status(), Status::Idle, "sanity check");
        assert!(
            !app.read_with(cx, |app, _| app.worktree_is_expanded(&idle_row)),
            "an idle-rooted worktree must default to collapsed"
        );
    }

    #[gpui::test]
    fn a_worktree_with_only_a_shell_open_produces_no_agent_row(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(repo.path().to_path_buf(), "repo")];
        });
        cx.run_until_parked();

        // The startup shell (`root::state`) already occupies this worktree - assert the
        // precondition rather than assuming it, then add a second shell explicitly so this test
        // doesn't depend on exactly how many the app happens to start with.
        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.agents
                    .iter()
                    .all(|agent| !agent.kind.is_agent_session()),
                "precondition: every open agent in this test is a shell"
            );
            assert!(
                app.agents.iter().count() >= 2,
                "precondition: at least two shells are genuinely open"
            );
        });

        let rows = app.read_with(cx, |app, cx| app.build_agent_rows(cx));
        assert!(
            rows.is_empty(),
            "a worktree with only shells open must produce zero agent rows - got {rows:?}"
        );

        let worktree_row = app.read_with(cx, |app, cx| {
            app.build_worktree_rows(cx)
                .into_iter()
                .find(|row| row.path == repo.path())
                .expect("the repo's own worktree must still produce a row")
        });
        assert!(
            worktree_row.agents.is_empty(),
            "and the worktree row itself must fold in none of them"
        );
        assert_eq!(
            worktree_row.aggregate_status(),
            Status::Idle,
            "a shell-only worktree aggregates exactly like an empty one"
        );
    }

    #[gpui::test]
    fn the_selected_worktree_never_idle_collapses_by_default(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let running_row = app.read_with(cx, |app, cx| {
            app.build_worktree_rows(cx)
                .into_iter()
                .find(|row| row.path == wt.path())
                .expect("the seeded worktree must produce a row")
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.current_worktree_path()),
            Some(wt.path().to_path_buf()),
            "premise: `wt` really is the currently selected worktree"
        );

        let idle_row = rail::WorktreeRow {
            agents: Vec::new(),
            ..running_row
        };
        assert_eq!(idle_row.aggregate_status(), Status::Idle, "sanity check");
        assert!(
            app.read_with(cx, |app, _| app.worktree_is_expanded(&idle_row)),
            "the selected worktree must stay expanded even once idle, with no explicit override"
        );

        // An explicit caret click still wins over the selection exemption - the user's own
        // choice to collapse the active worktree must be honored, not silently overridden back.
        app.update(cx, |app, cx| {
            app.toggle_worktree_collapsed(wt.path().to_path_buf(), true, cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app.worktree_is_expanded(&idle_row)),
            "an explicit collapse override must still win even for the selected worktree"
        );
    }

    #[gpui::test]
    fn toggle_worktree_collapsed_flips_and_remembers_the_override(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
        });
        // See the identical `run_until_parked` call/comment in
        // `worktree_is_expanded_defaults_to_the_real_idle_rooted_rule` just above.
        cx.run_until_parked();

        let row = app.read_with(cx, |app, cx| {
            app.build_worktree_rows(cx)
                .into_iter()
                .find(|row| row.path == wt.path())
                .expect("row")
        });
        let expanded_before = app.read_with(cx, |app, _| app.worktree_is_expanded(&row));
        assert!(
            expanded_before,
            "sanity check: a running row starts expanded"
        );

        app.update(cx, |app, cx| {
            app.toggle_worktree_collapsed(wt.path().to_path_buf(), expanded_before, cx);
        });
        assert!(
            !app.read_with(cx, |app, _| app.worktree_is_expanded(&row)),
            "one toggle must collapse an expanded-by-default row"
        );

        app.update(cx, |app, cx| {
            app.toggle_worktree_collapsed(wt.path().to_path_buf(), false, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.worktree_is_expanded(&row)),
            "a second toggle must restore the expanded state - a real remembered override, not \
             a one-shot flag"
        );
    }

    #[gpui::test]
    fn selecting_an_agent_selects_its_worktree_and_raises_its_tab(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let wt_b = tempfile::tempdir().expect("tempdir b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(wt_a.path().to_path_buf(), "wt-a"),
                worktree_item(wt_b.path().to_path_buf(), "wt-b"),
            ];
        });
        let agent_in_b = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });

        // Land back on worktree A before the click under test, so a passing assertion proves
        // the click itself moved the selection rather than it already pointing at B.
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        assert_eq!(app.read_with(cx, |app, _| app.selected), Some(0));

        app.update_in(cx, |app, window, cx| {
            app.select_agent(agent_in_b, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(1),
            "selecting an agent in worktree B must select worktree B in the rail"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.active_id()),
            Some(agent_in_b),
            "and that exact agent must become the active tab"
        );
    }

    #[gpui::test]
    fn render_rail_list_does_not_panic_across_bare_and_multi_agent_worktrees(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let bare_wt = tempfile::tempdir().expect("tempdir bare");
        let busy_wt = tempfile::tempdir().expect("tempdir busy");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(bare_wt.path().to_path_buf(), "bare-wt"),
                worktree_item(busy_wt.path().to_path_buf(), "busy-wt"),
            ];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
            app.agents.spawn(
                ProcessKind::claude(),
                busy_wt.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
            app.agents.spawn(
                ProcessKind::codex(),
                busy_wt.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
        });

        app.update(cx, |app, cx| {
            let groups = std::rc::Rc::new(app.build_repo_groups(cx));
            let _ = app.render_rail_list(&groups, cx);
        });
    }

    #[gpui::test]
    fn build_repo_groups_header_wt_count_is_unaffected_by_the_filter_query(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_alpha = tempfile::tempdir().expect("tempdir alpha");
        let wt_beta = tempfile::tempdir().expect("tempdir beta");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(wt_alpha.path().to_path_buf(), "alpha"),
                worktree_item(wt_beta.path().to_path_buf(), "beta"),
            ];
        });

        let groups_before_filter = app.update(cx, |app, cx| app.build_repo_groups(cx));
        assert_eq!(
            groups_before_filter[0].all_rows.len(),
            2,
            "sanity check: both real worktrees are counted before any filter is typed"
        );
        assert_eq!(
            groups_before_filter[0].rows.len(),
            2,
            "sanity check: both rows are displayed with an empty filter query"
        );

        app.update(cx, |app, _cx| {
            app.filter_query
                .insert_str("alpha", std::time::Instant::now());
        });

        let groups_after_filter = app.update(cx, |app, cx| app.build_repo_groups(cx));
        assert_eq!(
            groups_after_filter[0].all_rows.len(),
            2,
            "the header's `N wt` count must stay at the repo's real worktree count - typing \
             into the filter box must not shrink it"
        );
        assert_eq!(
            groups_after_filter[0].rows.len(),
            1,
            "sanity check: the *displayed* rows really did narrow to the one matching worktree \
             - proving the filter query took effect at all, just not on the header count"
        );
    }

    #[gpui::test]
    fn build_worktree_rows_shows_real_persisted_history_and_excludes_a_currently_live_agent(
        cx: &mut TestAppContext,
    ) {
        use crate::work_surface::agents::AgentKind;

        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });

        let no_history_row = app.update(cx, |app, cx| {
            app.build_worktree_rows(cx)
                .into_iter()
                .find(|row| row.path == wt.path())
                .expect("the seeded worktree must produce a row")
        });
        assert!(
            no_history_row.history.is_empty(),
            "premise: a worktree with no persisted history shows none"
        );

        let live_id = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::claude(),
                wt.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        let live_spawned_at = app.read_with(cx, |app, _| {
            app.agents
                .iter()
                .find(|agent| agent.id == live_id)
                .expect("the just-spawned agent")
                .spawned_at_unix
        });
        let live_key =
            crate::review::state::baseline_key(wt.path(), AgentKind::Claude, live_spawned_at);
        let closed_key =
            crate::review::state::baseline_key(wt.path(), AgentKind::Claude, 1_700_000_000);

        app.update(cx, |app, _cx| {
            // A real closed agent's persisted record, written through the real `set` the app's
            // own hook-status poll uses (`crate::hooks::flow::AdeApp::record_agent_statuses`).
            app.agent_status_state.set(
                closed_key.clone(),
                LiveRun::new(wt.path(), "Claude", 1_700_000_000, Status::Review)
                    .activity("Edit: src/auth.rs".to_owned())
                    .session_id("session-closed".to_owned()),
                1_700_000_500,
            );
            // A record for an agent that is *also* still open right now - the real case
            // `record_agent_statuses` produces for a live agent (it records every agent with a
            // fresh hook fact, not just closed ones). It must not show up twice.
            app.agent_status_state.set(
                live_key.clone(),
                LiveRun::new(wt.path(), "Claude", live_spawned_at, Status::Run),
                1_700_000_600,
            );
        });

        let rows = app.update(cx, |app, cx| app.build_worktree_rows(cx));
        let wt_row = rows
            .iter()
            .find(|row| row.path == wt.path())
            .expect("the seeded worktree must still produce a row");
        assert_eq!(
            wt_row.history.len(),
            1,
            "exactly the closed agent's record must show, not the live agent's own"
        );
        assert_eq!(wt_row.history[0].key, closed_key);
        assert_eq!(
            wt_row.history[0].session_id.as_deref(),
            Some("session-closed")
        );
    }

    #[gpui::test]
    fn resume_past_agent_with_a_real_session_id_spawns_a_real_claude_resume(
        cx: &mut TestAppContext,
    ) {
        use crate::work_surface::agents::AgentKind;

        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });

        let key = crate::review::state::baseline_key(wt.path(), AgentKind::Claude, 1);
        app.update(cx, |app, _cx| {
            app.agent_status_state.set(
                key.clone(),
                LiveRun::new(wt.path(), "Claude", 1, Status::Idle)
                    .session_id("5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned()),
                2,
            );
        });

        let resumed = app.update_in(cx, |app, window, cx| {
            app.resume_past_agent(&key, window, cx)
        });
        assert!(
            resumed,
            "a real, decodable record naming a real, known worktree must resume"
        );

        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(0),
            "resuming must select the record's own worktree"
        );

        let new_pane = app.read_with(cx, |app, _cx| {
            app.agents
                .iter_for_cwd(wt.path().to_path_buf())
                .last()
                .expect("a new agent must have been spawned into wt")
                .pane
                .clone()
        });
        let spec = new_pane.read_with(cx, |pane, _| pane.spec_for_test().clone());
        assert_eq!(spec.program, PathBuf::from("claude"));
        assert_eq!(
            spec.args[0..2],
            [
                "--resume".to_owned(),
                "5af4c210-34fa-4ab2-9c35-f6ceab76551c".to_owned()
            ],
            "the real session id must lead the resumed spawn's arguments"
        );
    }

    #[gpui::test]
    fn resume_past_agent_without_a_session_id_reopens_a_fresh_agent_instead(
        cx: &mut TestAppContext,
    ) {
        use crate::work_surface::agents::AgentKind;

        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });

        let key = crate::review::state::baseline_key(wt.path(), AgentKind::Codex, 1);
        app.update(cx, |app, _cx| {
            app.agent_status_state.set(
                key.clone(),
                LiveRun::new(wt.path(), "Codex", 1, Status::Idle),
                2,
            );
        });

        let resumed = app.update_in(cx, |app, window, cx| {
            app.resume_past_agent(&key, window, cx)
        });
        assert!(resumed);

        let new_pane = app.read_with(cx, |app, _cx| {
            app.agents
                .iter_for_cwd(wt.path().to_path_buf())
                .last()
                .expect("a new agent must have been spawned into wt")
                .pane
                .clone()
        });
        let spec = new_pane.read_with(cx, |pane, _| pane.spec_for_test().clone());
        assert_eq!(spec.program, PathBuf::from("codex"));
        assert!(
            spec.args.is_empty(),
            "with no real session id, the fallback must not fabricate a --resume flag - got \
             {:?}",
            spec.args
        );
    }

    #[gpui::test]
    fn resume_past_agent_is_a_no_op_for_an_unknown_key(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let count_before = app.read_with(cx, |app, _| app.agents.iter().count());

        let resumed = app.update_in(cx, |app, window, cx| {
            app.resume_past_agent("no-such-key", window, cx)
        });
        assert!(!resumed);
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.iter().count()),
            count_before,
            "a no-op resume must not spawn anything"
        );
    }
}

/// Real click-through coverage of the rail's repo groups against the live `AdeApp`/window,
/// mirroring `crate::code_surface::render`'s own `cx.simulate_click`-against-`debug_bounds`
/// technique - not just calling handlers directly, since what these tests pin down is the
/// *render* side's wiring (which elements have click handlers at all, and which deliberately
/// don't). The rail's click contract, per explicit user direction after two subtler repo-header
/// behaviors were both rejected: only worktree rows and agent rows are clickable; the repo
/// header and its `+` are plain, inert chrome; and only worktrees have tabs. Repo switching
/// happens exclusively through a worktree row under the target repo's group
/// (`crate::root::AdeApp::select_worktree_by_path`'s cross-repo fallback).
#[cfg(test)]
mod repo_checkout_tests {
    use crate::root::focus::palette_focus_tests;
    use crate::work_surface::agents::ProcessKind;
    use crate::work_surface::state::TabRef;
    use gpui::TestAppContext;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    /// A minimal, real `git init`-ed repo - mirrors `crate::root::state::
    /// load_worktrees_integration_tests`'s identical own helper, duplicated locally per this
    /// crate's own established per-test-module convention rather than shared.
    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
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

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    #[gpui::test]
    fn clicking_a_non_focused_repos_header_does_nothing_at_all(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir a");
        let repo_b = tempfile::tempdir().expect("tempdir b");
        std::fs::write(repo_b.path().join("b.txt"), "b\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));
        assert!(
            groups.iter().any(|group| group.repo_id == repo_b_id),
            "sanity check: repo B's group must still render at all (GitHub issue #113) - an \
             unclickable header still paints"
        );

        let (focused_before, tree_root_before, selected_before, active_before, agents_before) = app
            .read_with(cx, |app, _| {
                (
                    app.focused_repo_path(),
                    app.file_tree_root.clone(),
                    app.selected,
                    app.agents.active_id(),
                    app.agents.iter().count(),
                )
            });
        assert_eq!(
            focused_before,
            repo_a.path(),
            "sanity check: repo B is not the focused repo before the click"
        );

        // A real click on repo B's header's own painted bounds, not a direct method call - what
        // this pins down is precisely that the render side attaches no handler.
        let selector: &'static str =
            Box::leak(format!("repo-group-header-{}", repo_b_id.0).into_boxed_str());
        let header_bounds = cx
            .debug_bounds(selector)
            .expect("repo B's header must have painted with a real debug selector");
        cx.simulate_click(header_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                focused_before,
                "clicking a repo header must not switch the focused repo - the header is not a \
                 click target; only worktree and agent rows are"
            );
            assert_eq!(
                app.file_tree_root, tree_root_before,
                "and it must not re-root or reload the file tree either - not \"pure \
                 navigation\", nothing"
            );
            assert_eq!(
                app.selected, selected_before,
                "no worktree selection may change"
            );
            assert_eq!(
                app.agents.active_id(),
                active_before,
                "no tab/agent may activate or deactivate"
            );
            assert_eq!(
                app.agents.iter().count(),
                agents_before,
                "and nothing may be spawned or closed"
            );
        });
    }

    #[gpui::test]
    fn build_repo_groups_marks_a_non_focused_repos_data_as_loaded_once_its_real_fetch_resolves(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir a");
        let repo_b = init_repo();

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_a_id = app.read_with(cx, |app, _| {
            app.focused_repo()
                .expect("sanity check: repo A is focused")
                .id
        });

        // `add_repo` and `build_repo_groups` run inside the same synchronous `update` call, with
        // no `run_until_parked` in between - the real background fetch `add_repo` kicks off has
        // had no chance to run at all yet, so this genuinely observes the pre-fetch state rather
        // than racing it.
        let (repo_b_id, groups_before_fetch) = app.update(cx, |app, cx| {
            let id = app.add_repo(repo_b.path().to_path_buf(), cx);
            let groups = app.build_repo_groups(cx);
            (id, groups)
        });

        let group_b_before = groups_before_fetch
            .iter()
            .find(|g| g.repo_id == repo_b_id)
            .expect("repo B's group must exist immediately, even before its fetch resolves");
        assert!(
            !group_b_before.rows_loaded,
            "repo B's real fetch hasn't resolved yet - rows_loaded must still be false"
        );
        assert!(
            group_b_before.all_rows.is_empty(),
            "and its rows must genuinely be empty until that fetch resolves, never a fabricated \
             non-empty guess in the meantime"
        );

        cx.run_until_parked();

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));

        let group_a = groups
            .iter()
            .find(|g| g.repo_id == repo_a_id)
            .expect("repo A's group must exist");
        assert!(
            group_a.rows_loaded,
            "the focused repo's own data path is unchanged by this feature - always loaded"
        );

        let group_b = groups
            .iter()
            .find(|g| g.repo_id == repo_b_id)
            .expect("repo B's group must exist");
        assert!(
            group_b.rows_loaded,
            "repo B's real background fetch has resolved by now (`run_until_parked`) - \
             rows_loaded must become true even though repo B was never focused"
        );
        assert_eq!(
            group_b.all_rows.len(),
            1,
            "a real git repo always has at least its own main checkout as a worktree - this \
             must read as a real 1, never a false 0 masquerading as \"confirmed empty\""
        );
    }

    #[gpui::test]
    fn build_repo_groups_folds_a_non_focused_repos_real_agent_into_its_own_row(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir a");
        let repo_b = init_repo();

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        // Focus repo B long enough to spawn a real Claude agent into it, mirroring how a user
        // would actually get one running there - then switch straight back to repo A.
        let agent_id = app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
            app.agents.spawn(
                ProcessKind::claude(),
                repo_b.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_a.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_a.path(),
                "sanity check: repo A is focused again"
            );
            assert!(
                app.agents.iter().any(|agent| agent.id == agent_id),
                "repo B's real agent must still genuinely exist - not closed by the switch back \
                 to repo A"
            );
        });

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));
        let repo_b_id = app.read_with(cx, |app, _| {
            app.repos
                .iter()
                .find(|repo| repo.path == repo_b.path())
                .expect("repo B must still be a known repo")
                .id
        });
        let group_b = groups
            .iter()
            .find(|group| group.repo_id == repo_b_id)
            .expect("repo B's group must still render while unfocused");
        let worktree_row = group_b
            .all_rows
            .iter()
            .find(|row| row.path == repo_b.path())
            .expect("repo B's own root worktree must still be a real row");
        assert_eq!(
            worktree_row.agents.len(),
            1,
            "repo B's real agent must be folded into its own worktree row even while unfocused"
        );
        assert_eq!(
            worktree_row.agents[0].id, agent_id,
            "the folded-in row must be the exact same agent, not a stand-in"
        );
        assert_eq!(
            worktree_row.agents[0].status,
            crate::rail::status::Status::Run,
            "the folded-in row must carry this agent's own genuinely live status, not a \
             fabricated default"
        );

        app.read_with(cx, |app, _| {
            assert!(
                !app.combined_tab_order().contains(&TabRef::Agent(agent_id)),
                "repo A's tab strip must not show repo B's agent - cross-repo visibility lives \
                 in the rail alone"
            );
        });
    }

    #[gpui::test]
    fn clicking_the_already_focused_repos_header_does_not_reset_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let repo_id = app.read_with(cx, |app, _| {
            app.focused_repo()
                .expect("sanity check: a repo is focused")
                .id
        });

        app.update(cx, |app, cx| {
            app.commit_menu_open = true;
            cx.notify();
        });
        cx.run_until_parked();

        let selector: &'static str =
            Box::leak(format!("repo-group-header-{}", repo_id.0).into_boxed_str());
        let header_bounds = cx
            .debug_bounds(selector)
            .expect("the focused repo's own header must still paint");
        cx.simulate_click(header_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "re-clicking the already-focused repo's own header must not reset its live UI state"
        );
    }

    /// Same linked-worktree idiom the sibling test modules use: created with no new commits of its
    /// own, which is all these tests need from it (a second, real, selectable worktree row).
    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> std::path::PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        path
    }

    #[gpui::test]
    fn clicking_a_non_focused_repos_worktree_row_switches_repo_and_selects_it(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let repo_b_feature = add_worktree(repo_b.path(), "feature", "b-feature");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_a.path(),
                "sanity check: repo A is the focused repo, so repo B's worktrees are genuinely \
                 absent from `app.worktrees`"
            );
            assert!(
                !app.worktrees.iter().any(|item| item.path == repo_b_feature),
                "sanity check: the row about to be clicked is not in the focused repo's own list \
                 - which is exactly why the old lookup missed it"
            );
        });

        // The row must genuinely have painted for repo B, from repo B's own `Repo::worktrees` -
        // that is what made this click reachable (and this bug reachable) in the first place.
        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));
        let index = groups
            .iter()
            .find(|group| group.repo_name == repo_b.path().file_name().unwrap().to_string_lossy())
            .expect("repo B's group must exist")
            .rows
            .iter()
            .position(|row| row.path == repo_b_feature)
            .expect("repo B's linked worktree must be a real, rendered row");
        let selector: &'static str = Box::leak(
            format!("worktree-row-{index}-{}", repo_b_feature.display()).into_boxed_str(),
        );
        let row_bounds = cx
            .debug_bounds(selector)
            .expect("repo B's linked worktree row must have painted");
        cx.simulate_click(row_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_b.path(),
                "clicking a worktree row under a non-focused repo must really switch focus to \
                 that repo, not silently no-op"
            );
            let selected = app
                .selected
                .and_then(|index| app.worktrees.get(index))
                .map(|item| item.path.clone());
            assert_eq!(
                selected,
                Some(repo_b_feature.clone()),
                "and the specific worktree that was clicked must be the selected one - not just \
                 the repo's main checkout"
            );
            assert_eq!(
                app.current_worktree_path(),
                Some(repo_b_feature.clone()),
                "the whole point of the switch: new work now targets the clicked worktree"
            );
            assert_eq!(
                app.file_tree_root, repo_b_feature,
                "and the real repo-scoped reload must have re-rooted at it, proving this went \
                 through the same real switch machinery a same-repo selection uses"
            );
        });

        let _ = std::fs::remove_dir_all(&repo_b_feature);
    }

    #[gpui::test]
    fn a_cross_repo_worktree_selection_survives_the_repos_own_background_fetch(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        // Two linked worktrees, so the target is not at index 0 and a stale index would be
        // visible as a wrong selection rather than accidentally landing on the right row.
        let _first = add_worktree(repo_b.path(), "first", "b-first");
        let target = add_worktree(repo_b.path(), "second", "b-second");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });
        cx.run_until_parked();

        // The handler directly this time - `run_until_parked` afterwards is what lets the real
        // fetch land on top of the synchronous seed.
        app.update_in(cx, |app, window, cx| {
            app.select_worktree_by_path(&target, window, cx);
        });
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.selected
                    .and_then(|i| app.worktrees.get(i))
                    .map(|w| &w.path),
                Some(&target),
                "the selection must be real immediately, from the seeded list - not deferred to \
                 whenever a background fetch happens to resolve"
            );
        });

        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.selected
                    .and_then(|i| app.worktrees.get(i))
                    .map(|w| &w.path),
                Some(&target),
                "and it must still be the selection once repo B's own real fetch has landed and \
                 replaced the seeded list"
            );
            assert!(
                app.worktree_selection_notice.is_none(),
                "nothing fell back to main, so no fallback notice may have been raised"
            );
        });

        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_dir_all(&_first);
    }

    #[gpui::test]
    fn selecting_a_worktree_path_no_repo_knows_about_still_does_nothing(cx: &mut TestAppContext) {
        let repo_a = init_repo();
        let repo_b = init_repo();

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });
        cx.run_until_parked();

        let gone = repo_b.path().join("worktree-that-never-existed");
        app.update_in(cx, |app, window, cx| {
            app.select_worktree_by_path(&gone, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_a.path(),
                "an unknown path must not switch repos"
            );
            assert_eq!(app.file_tree_root, repo_a.path());
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    fn an_agent_spawned_in_a_repo_opened_through_a_symlink_still_appears_in_the_rail(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let link_holder = TempDir::new().expect("tempdir");
        let link = link_holder.path().join("repo-link");
        std::os::unix::fs::symlink(repo.path(), &link).expect("symlink");
        assert_ne!(
            link,
            repo.path(),
            "sanity check: the symlink really is a different path from the real repo"
        );

        let (app, cx) = palette_focus_tests::open_test_app(cx, link.clone());
        cx.run_until_parked();

        let agent_id = app.update_in(cx, |app, window, cx| {
            app.new_agent(ProcessKind::claude(), window, cx);
            app.agents
                .active()
                .expect("New agent must really spawn an agent")
                .id
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo.path(),
                "the repo path must be stored fully resolved, not as the symlink it was opened \
                 through - every per-worktree lookup in this app compares it against git's own \
                 resolved paths by exact equality"
            );
        });

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));
        let group = groups.first().expect("the repo's group must exist");
        let row = group
            .all_rows
            .iter()
            .find(|row| row.path == repo.path())
            .expect("the repo's own main checkout must be a real worktree row");
        assert_eq!(
            row.agents.len(),
            1,
            "the freshly spawned agent must be folded into its own worktree's row - before this \
             fix its cwd was the unresolved symlink path, which matched no row at all and made \
             the agent vanish from the rail completely"
        );
        assert_eq!(row.agents[0].id, agent_id);
    }

    #[cfg(unix)]
    #[gpui::test]
    fn opening_a_symlinked_folder_stores_the_resolved_repo_path(cx: &mut TestAppContext) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let link_holder = TempDir::new().expect("tempdir");
        let link = link_holder.path().join("repo-b-link");
        std::os::unix::fs::symlink(repo_b.path(), &link).expect("symlink");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(link.clone(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_b.path(),
                "Open Folder… on a symlinked directory must store the resolved repo path"
            );
            assert!(
                app.agents
                    .iter_for_cwd(repo_b.path().to_path_buf())
                    .next()
                    .is_some(),
                "and the shell it opens there must run in that same resolved path, so it folds \
                 into the repo's own worktree row in the rail"
            );
        });

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));
        let row = groups
            .iter()
            .flat_map(|group| group.all_rows.iter())
            .find(|row| row.path == repo_b.path())
            .expect("repo B's own main checkout must be a real worktree row");
        assert_eq!(
            row.path,
            repo_b.path(),
            "sanity check: the row is keyed by git's own resolved path"
        );
    }
}

/// The reported "at the start of the program you select something and a tab bar has a terminal;
/// then I select a worktree and this is lost" - and the architectural invariant that replaced the
/// family of bugs behind it:
#[cfg(test)]
mod worktree_tab_attribution_tests {
    use crate::root::focus::palette_focus_tests;
    use crate::work_surface::state::TabRef;
    use gpui::TestAppContext;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
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

    /// A real repository with a real commit - `wt_core::list_worktrees_porcelain` reports nothing
    /// at all for a bare `tempfile::tempdir()`, so these tests need a genuine main worktree row.
    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        path
    }

    /// Clicks the real, painted rail row for `worktree_path`, exactly as a user would - resolving
    /// the row's own `debug_selector` from the very `build_repo_groups` output it was rendered
    /// from, the same idiom `repo_checkout_tests::
    /// clicking_a_non_focused_repos_worktree_row_switches_repo_and_selects_it` established.
    fn click_worktree_row(
        app: &gpui::Entity<crate::root::AdeApp>,
        cx: &mut gpui::VisualTestContext,
        worktree_path: &Path,
    ) {
        let index = app
            .update(cx, |app, cx| app.build_repo_groups(cx))
            .iter()
            .find_map(|group| group.rows.iter().position(|row| row.path == worktree_path))
            .expect("the worktree must be a real, rendered rail row");
        let selector: &'static str =
            Box::leak(format!("worktree-row-{index}-{}", worktree_path.display()).into_boxed_str());
        let bounds = cx
            .debug_bounds(selector)
            .expect("the worktree row must have painted");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_fresh_launch_genuinely_selects_the_repos_own_main_worktree(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let selected = app
                .selected
                .and_then(|index| app.worktrees.get(index))
                .map(|item| item.path.clone());
            assert_eq!(
                selected,
                Some(repo.path().to_path_buf()),
                "a fresh launch must land on the repo's own main worktree as a real selection - \
                 `AdeApp::selected` staying `None` here is the reported bug, not a neutral \
                 starting state"
            );
            assert_eq!(
                app.current_worktree_path(),
                Some(repo.path().to_path_buf()),
                "and it must be a real selection, not `current_worktree_path`'s old repo-root fallback"
            );
            let startup_shell_cwds: Vec<PathBuf> =
                app.agents.iter().map(|agent| agent.cwd.clone()).collect();
            assert_eq!(
                startup_shell_cwds,
                vec![repo.path().to_path_buf()],
                "the guaranteed startup shell must have been spawned into that same, genuinely \
                 selected worktree"
            );
            assert_eq!(
                app.combined_tab_order().len(),
                1,
                "and its tab must be the one thing the strip shows"
            );
        });
    }

    #[gpui::test]
    fn the_startup_terminal_is_a_real_worktrees_tab_and_survives_switching_away_and_back(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let startup_agent = app.read_with(cx, |app, _| {
            let agents: Vec<_> = app.agents.iter().map(|agent| agent.id).collect();
            assert_eq!(agents.len(), 1, "exactly one startup shell");
            agents[0]
        });

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.current_worktree_path(),
                Some(repo.path().to_path_buf()),
                "premise: the startup terminal's own worktree must be genuinely selected before \
                 the user ever clicks anything - that is what makes the switch below a \
                 reversible navigation rather than an unexplained loss"
            );
            assert_eq!(
                app.combined_tab_order(),
                vec![TabRef::Agent(startup_agent)],
                "and the startup shell must be that worktree's own single tab"
            );
        });

        // Step 2: a real click on the linked worktree's row - an honest switch to a worktree that
        // has no tabs of its own yet.
        click_worktree_row(&app, cx, &feature);
        app.read_with(cx, |app, cx| {
            assert_eq!(
                app.current_worktree_path(),
                Some(feature.clone()),
                "the clicked worktree must be the selected one"
            );
            assert!(
                app.combined_tab_order().is_empty(),
                "and it genuinely has no tabs - an honestly empty strip, not a fabricated one"
            );
            let shell = app
                .agents
                .iter()
                .find(|agent| agent.id == startup_agent)
                .expect("the startup shell must still exist - switching worktrees never kills it");
            assert!(
                shell.pane.read(cx).is_running(),
                "and it must still be a real, live process, merely not the shown one"
            );
        });

        click_worktree_row(&app, cx, repo.path());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.combined_tab_order(),
                vec![TabRef::Agent(startup_agent)],
                "clicking back to the main worktree must restore the exact same startup shell \
                 tab - the process was never lost, only unshown"
            );
            assert_eq!(
                app.agents.active_id(),
                Some(startup_agent),
                "and the centre pane must genuinely be showing it again"
            );
        });
    }

    #[gpui::test]
    fn launching_against_a_subdirectory_still_attributes_its_shell_to_a_real_worktree(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        std::fs::create_dir_all(repo.path().join("crates")).expect("mkdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().join("crates"));
        cx.run_until_parked();

        let startup_agent = app.read_with(cx, |app, _| {
            assert_eq!(
                app.current_worktree_path(),
                Some(repo.path().to_path_buf()),
                "a subdirectory is not a worktree - this must land on the repo's real main \
                 worktree rather than on the subdirectory itself"
            );
            let agents: Vec<_> = app
                .agents
                .iter()
                .map(|agent| (agent.id, agent.cwd.clone()))
                .collect();
            assert_eq!(
                agents.len(),
                1,
                "the guaranteed startup shell must still exist"
            );
            assert_eq!(
                agents[0].1,
                repo.path().to_path_buf(),
                "and it must have been spawned into the real main worktree, not the \
                 subdirectory - a shell whose cwd matches no worktree row can never be reached \
                 from the rail again"
            );
            agents[0].0
        });

        // The row that reads as selected must be a real one, and clicking it must be a no-op that
        // keeps the tab - the exact click that used to orphan it forever.
        click_worktree_row(&app, cx, repo.path());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.combined_tab_order(),
                vec![TabRef::Agent(startup_agent)],
                "clicking the worktree the startup shell actually belongs to must keep its tab, \
                 not make it unreachable"
            );
        });
    }

    #[gpui::test]
    fn launching_inside_a_linked_worktree_selects_that_worktree_not_main(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature");
        let (app, cx) = palette_focus_tests::open_test_app(cx, feature.clone());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.current_worktree_path(),
                Some(feature.clone()),
                "the worktree the user actually pointed at must be the selected one"
            );
            assert_eq!(
                app.agents.iter().map(|a| a.cwd.clone()).collect::<Vec<_>>(),
                vec![feature.clone()],
                "and the startup shell must live in it"
            );
        });
    }

    #[gpui::test]
    fn opening_a_folder_lands_on_a_real_worktree_and_spawns_its_shell_there(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let selected = app
                .selected
                .and_then(|index| app.worktrees.get(index))
                .map(|item| item.path.clone());
            assert_eq!(
                selected,
                Some(repo_b.path().to_path_buf()),
                "opening a folder must land on that repo's own main worktree as a real selection"
            );
            assert_eq!(
                app.combined_tab_order().len(),
                1,
                "and the shell it guarantees must be that worktree's own single tab"
            );
            assert!(
                app.agents
                    .iter()
                    .any(|agent| agent.cwd == repo_b.path()
                        && Some(agent.id) == app.agents.active_id()),
                "and the active tab must genuinely be an agent in repo B's main worktree"
            );
        });
    }

    #[gpui::test]
    fn a_worktree_click_racing_the_open_fetch_still_gets_its_guaranteed_shell(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let feature = add_worktree(repo_b.path(), "feature", "feature");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();
        // Repo B is already a known repo with a real, already-fetched worktree list - which is
        // what makes the racing click below able to resolve a row at all.
        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
            // Deliberately no `run_until_parked` between these two: this is the whole race - the
            // click lands while the opening fetch is genuinely still in flight.
            app.select_worktree_by_path(&feature, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.current_worktree_path(),
                Some(feature.clone()),
                "the worktree the user actually clicked must win over the one the in-flight open \
                 fetch would have picked on its own"
            );
            assert!(
                app.agents.iter().any(|agent| agent.cwd == feature),
                "and the window's guaranteed initial shell must still have been spawned - into \
                 the raced-to worktree. Skipping it here (because `recover_selection` reported \
                 `Unchanged` rather than `NoPriorSelection`) would leave a freshly opened repo \
                 with no terminal at all"
            );
            assert!(
                !app.combined_tab_order().is_empty(),
                "so the tab strip genuinely shows it"
            );
        });
    }

    #[gpui::test]
    fn nothing_selected_means_nothing_shown_anywhere(cx: &mut TestAppContext) {
        let repo_a = init_repo();
        let repo_b = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        // Repo B enters through the real "Open Folder…" gesture, so it genuinely has a live shell
        // in its own main worktree - the precondition that made the old disagreement visible.
        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        let repo_a_id = app.read_with(cx, |app, _| {
            app.repos
                .iter()
                .find(|repo| repo.path == repo_a.path())
                .expect("repo A is known")
                .id
        });
        let repo_b_id = app.read_with(cx, |app, _| {
            app.repos
                .iter()
                .find(|repo| repo.path == repo_b.path())
                .expect("repo B is known")
                .id
        });
        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_a_id, window, cx);
        });
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, cx| {
            assert_eq!(app.selected, None, "premise: nothing is selected");
            assert_eq!(
                app.current_worktree_path(),
                None,
                "so there is genuinely no active worktree - not the repo root standing in for one"
            );
            assert!(
                app.combined_tab_order().is_empty(),
                "the tab strip must therefore be honestly empty, even though repo B really does \
                 have a live shell in its own root: that shell belongs to a worktree nobody has \
                 selected"
            );
            assert_eq!(
                app.agents.active_id(),
                None,
                "and the centre pane must be showing nothing - the state the tab strip used to \
                 contradict"
            );
            let any_row_selected = app
                .build_worktree_rows(cx)
                .iter()
                .any(|row| app.current_worktree_path().as_deref() == Some(row.path.as_path()));
            assert!(
                !any_row_selected,
                "and no rail row may read as selected either - the rail used to light up the \
                 main-worktree row here purely because `current_worktree_path` fell back to the repo \
                 root"
            );
            assert!(
                app.agents
                    .iter()
                    .any(|agent| agent.cwd == repo_b.path() && agent.pane.read(cx).is_running()),
                "repo B's own shell must still be a real, live background process throughout - \
                 cross-repo agent persistence is untouched by any of this"
            );
        });
    }
}

/// GitHub issue #45 ("Input blink only on focused input or file") plus a live follow-up report:
/// the rail filter's caret used to be a fixed trailing child, painted *after* the placeholder
/// text whenever `filter_query` was empty, instead of at the real cursor position (0). Real
/// interaction coverage, mirroring `crate::palette::render::palette_caret_tests`' own
/// measured-bounds technique rather than only reading the render code.
#[cfg(test)]
mod rail_filter_caret_tests {
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::time::Duration;

    #[gpui::test]
    fn caret_sits_before_the_placeholder_when_empty_and_after_the_text_once_typed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.run_until_parked();

        let empty_caret = cx
            .debug_bounds("rail-filter-caret")
            .expect("the caret should have really painted with an empty filter");
        let placeholder = cx
            .debug_bounds("rail-filter-text")
            .expect("the placeholder text should have really painted");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "with an empty filter, the real caret must sit before (at or left of) the \
             placeholder's own start x, not after it - got caret {:?} vs placeholder {:?}",
            empty_caret,
            placeholder,
        );

        cx.simulate_input("main");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "main",
            "sanity check: real typed filter"
        );

        let typed_caret = cx
            .debug_bounds("rail-filter-caret")
            .expect("the caret should have really painted with a typed filter");
        let typed_text = cx
            .debug_bounds("rail-filter-text")
            .expect("the real typed text should have really painted");
        assert!(
            typed_caret.origin.x >= typed_text.origin.x + typed_text.size.width,
            "with a typed filter, the real caret must sit at or after the typed text's own \
             right edge, not before it - got caret {:?} vs text {:?}",
            typed_caret,
            typed_text,
        );
        assert!(
            typed_caret.origin.x > empty_caret.origin.x,
            "the caret's real measured horizontal position must differ between the \
             empty-filter state (before the placeholder) and a typed-filter state (after the \
             real text) - got {:?} vs {:?}",
            empty_caret.origin.x,
            typed_caret.origin.x,
        );
    }

    #[gpui::test]
    fn focusing_the_rail_filter_starts_the_real_shared_blink_loop(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        // `on_focus`/`on_blur` (`AdeApp::wire_caret_blink`'s own mechanism) only fire while GPUI
        // considers the window itself "active" - a real, freshly opened test window starts out
        // not active at all.
        app.update_in(cx, |_app, window, _cx| window.activate_window());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.simulate_input("m");
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "a fresh focus must start solid/visible"
        );

        cx.background_executor.advance_clock(
            crate::root::caret_blink::CARET_BLINK_INTERVAL + Duration::from_millis(50),
        );
        cx.run_until_parked();
        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "focusing the rail filter must have started the real, live shared blink task - if \
             `filter_focus_handle` were never wired into `AdeApp::wire_caret_blink`, no timer \
             would be running at all and this flag would still be stuck solid"
        );
    }
}

/// GitHub issue #5's "custom icon packs" - real coverage that the rail agent row's chip
/// (`AdeApp::render_agent_chip_icon`, the one real call site this feature is wired to today)
/// actually switches between the app's default letter chip and a real pack icon, rather than
/// just trusting the render code's own claim to do so. GitHub issue #309's own regression test
/// lives here too - see `the_pack_icon_element_is_a_real_image_not_a_colour_dependent_svg`.
#[cfg(test)]
mod agent_chip_icon_pack_tests {
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use crate::work_surface::agents::ProcessKind;
    use gpui::{px, TestAppContext};

    fn worktree_item(path: std::path::PathBuf, label: &str) -> WorktreeItem {
        WorktreeItem {
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

    /// A real, running agent in a real seeded worktree - a just-spawned agent is `Status::Run`,
    /// which defaults its worktree row to expanded (`AdeApp::worktree_is_expanded`'s own
    /// "idle-rooted" rule), so the agent row (and this chip) actually renders without needing a
    /// separate collapse-override hack.
    fn open_with_a_running_agent(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        gpui::Entity<crate::root::AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::claude(),
                wt.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        (repo, wt, app, cx)
    }

    #[gpui::test]
    fn the_rail_agent_row_shows_the_default_chip_with_no_pack_configured(cx: &mut TestAppContext) {
        let (_repo, _wt, _app, cx) = open_with_a_running_agent(cx);

        assert!(
            cx.debug_bounds("agent-chip-icon-default").is_some(),
            "with no icon pack configured, the rail's own default agent chip must paint"
        );
        assert!(
            cx.debug_bounds("agent-chip-icon-pack-image").is_none(),
            "with no icon pack configured, no pack image element must paint at all"
        );
    }

    #[gpui::test]
    fn the_rail_agent_row_switches_to_a_real_pack_icon_once_one_is_configured(
        cx: &mut TestAppContext,
    ) {
        let pack_dir = tempfile::tempdir().expect("tempdir");
        // The seeded agent is a real `AgentKind::Claude` (`work_surface::agent_icon_name`'s own
        // mapping), so `claude.svg` is the real file this specific row's chip looks for.
        std::fs::write(pack_dir.path().join("claude.svg"), "<svg></svg>").expect("write");

        let (_repo, _wt, app, cx) = open_with_a_running_agent(cx);
        app.update(cx, |app, cx| {
            app.settings.icon_pack.directory = Some(pack_dir.path().to_path_buf());
            cx.notify();
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("agent-chip-icon-pack-image").is_some(),
            "once a real pack directory with a matching claude.svg is configured, the rail row \
             must really switch to painting the pack's own image element"
        );
        assert!(
            cx.debug_bounds("agent-chip-icon-default").is_none(),
            "the default letter chip must not also paint once the pack icon takes over - \
             exactly one of the two must be showing, never both at once"
        );
    }

    #[gpui::test]
    fn the_pack_icon_element_is_a_real_image_not_a_colour_dependent_svg(cx: &mut TestAppContext) {
        let pack_dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(pack_dir.path().join("claude.svg"), "<svg></svg>").expect("write");

        let (_repo, _wt, app, cx) = open_with_a_running_agent(cx);
        app.update(cx, |app, cx| {
            app.settings.icon_pack.directory = Some(pack_dir.path().to_path_buf());
            cx.notify();
        });
        cx.run_until_parked();

        let mut element = app.read_with(cx, |app, _cx| {
            app.render_agent_chip_icon(ProcessKind::claude(), px(15.0), app.ui_text_size(9.0))
        });
        assert!(
            element.downcast_mut::<gpui::Img>().is_some(),
            "a configured pack icon must be built through gpui::img(), whose paint never \
             consults style.text.color, not gpui::svg(), whose paint silently skips painting \
             altogether when no text colour is set"
        );
        assert!(
            element.downcast_mut::<gpui::Svg>().is_none(),
            "a configured pack icon must not be a gpui::svg() element - that is the exact shape \
             of GitHub issue #309's empty-box bug"
        );
    }
}

/// Revision 6's status rename (`design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4g,
/// GitHub issue #280): [`Status::Review`] renders as `Finished`/`finished`, and the old
/// `Review ready` wording must never come back to any rendered surface.
#[cfg(test)]
mod status_wording_tests {
    use super::*;
    use crate::sound::SoundEventKind;
    use crate::work_surface::state::{footer_actions, pty_state_label};
    use std::time::Duration;

    fn row(status: Status, review_file_count: Option<usize>) -> AgentRow {
        AgentRow {
            id: 1,
            kind: ProcessKind::claude(),
            title: "agent-1".to_string(),
            cwd: PathBuf::from("/wt-1"),
            status,
            branch: Some("feature-x".to_string()),
            add: 0,
            del: 0,
            exit_code: Some(0),
            activity: None,
            elapsed: Duration::from_secs(90),
            review_file_count,
        }
    }

    /// Every user-visible string this app derives from a [`Status`], gathered from the real
    /// functions that produce them: the shared [`Status::label`] (work-surface context-bar
    /// status pill), the rail agent row's own state word and trailing text, the rail history
    /// row's `was <state>` line, the agent context bar's footer action labels, the terminal
    /// pane header's pty-state text, and the Sounds settings page's row label/hint for the
    /// event this status raises.
    fn every_rendered_status_string() -> Vec<String> {
        let mut wording: Vec<String> = Vec::new();
        for status in Status::ORDER {
            wording.push(status.label().to_string());
            wording.push(agent_state_word(status).to_string());
            wording.push(format!("was {}", agent_state_word(status)));
            for count in [None, Some(0), Some(1), Some(12)] {
                wording.push(agent_trailing_text(&row(status, count)));
            }
            for action in footer_actions(status) {
                wording.push(action.label.to_string());
            }
            for is_running in [true, false] {
                for exit_code in [None, Some(0), Some(1)] {
                    wording.push(pty_state_label(is_running, status, exit_code));
                }
            }
        }
        for event in SoundEventKind::ALL {
            wording.push(event.label().to_string());
            wording.push(event.description().to_string());
        }
        wording
    }

    #[test]
    fn no_rendered_status_string_anywhere_says_review_ready() {
        for text in every_rendered_status_string() {
            assert!(
                !text.to_lowercase().contains("review ready"),
                "revision 6 renamed this status to 'Finished' - no rendered label, state word \
                 or tooltip may say 'review ready' again, got {text:?}"
            );
        }
    }

    #[test]
    fn the_finished_status_renders_the_rev6_words_in_both_cases() {
        assert_eq!(
            Status::Review.label(),
            "Finished",
            "the shared status label (work-surface context bar, and everywhere else \
             `Status::label` is rendered) must be the rev-6 word"
        );
        assert_eq!(
            agent_state_word(Status::Review),
            "finished",
            "the rail agent row's state word is the lowercase form, per §2.3's state-word \
             convention"
        );
    }

    #[test]
    fn a_finished_agent_shows_its_file_count_beside_the_word_and_nothing_when_unmeasured() {
        assert_eq!(
            agent_trailing_text(&row(Status::Review, None)),
            "",
            "an unmeasured file count must render nothing at all, never a fabricated 0"
        );
        assert_eq!(
            agent_trailing_text(&row(Status::Review, Some(0))),
            "0 files"
        );
        assert_eq!(agent_trailing_text(&row(Status::Review, Some(1))), "1 file");
        assert_eq!(
            agent_trailing_text(&row(Status::Review, Some(12))),
            "12 files"
        );
    }
}

/// Revision 6's rail corrections (GitHub issue #289, `design_handoff_jerry_ade/revision 5/
/// REVISION-2026-08-14.md` §4 and `STAGE-A-CHANGELOG.md` §4k-§4s), in their pure, window-free
/// half: the two colour decisions that were structurally wrong before, and the deletion of the
/// rail's ask card.
#[cfg(test)]
mod rail_correction_tests {
    use super::*;

    #[test]
    fn the_worktree_edge_is_selection_or_nothing_whatever_the_row_holds() {
        for status in Status::ORDER {
            for is_prunable in [false, true] {
                assert_eq!(
                    worktree_row_edge(status, is_prunable, false),
                    None,
                    "an unselected row must draw no edge at all - not a status colour, not the \
                     old prunable grey, not a bare grey (status {status:?}, prunable \
                     {is_prunable})"
                );
                assert_eq!(
                    worktree_row_edge(status, is_prunable, true),
                    Some(theme::border::SELECTED_EDGE),
                    "the selected row's edge is the app's one selection blue, whatever its \
                     agents are doing (status {status:?}, prunable {is_prunable})"
                );
            }
        }
    }

    #[test]
    fn the_agent_title_sits_below_its_parent_branch_in_the_hierarchy() {
        assert_eq!(
            agent_title_color(Status::Run, false),
            theme::rail::AGENT_TITLE,
            "a live agent's title is the rail's own agent-title token, one step below the \
             branch's `text::STRONG`"
        );
        assert_ne!(
            agent_title_color(Status::Run, false),
            theme::text::STRONG,
            "the child must not share the parent's colour - equal brightness one level down is \
             the exact defect §4n names"
        );
        assert_eq!(
            agent_title_color(Status::Idle, false),
            theme::text::DIMMER,
            "a paused agent drops further still"
        );
        assert_eq!(
            agent_title_color(Status::Fail, true),
            theme::text::SELECTED,
            "the globally active agent's title is the selection colour whatever its status"
        );
    }

    #[test]
    fn no_rail_render_code_reaches_for_the_removed_ask_card() {
        const SOURCE: &str = include_str!("render.rs");
        // Comment lines (doc comments included) are dropped: this file explains the removal in
        // prose in three places, and prose naming a thing is not code reaching for it.
        let code = SOURCE
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        // Both needles are assembled at runtime rather than written as literals: this test's own
        // source is inside the string it scans, so a literal would match itself.
        let card_tokens = format!("{}{}", "ASK_", "CARD");
        let removed_field = format!("{}{}", "question_", "preview");
        assert!(
            !code.contains(&card_tokens),
            "the rail's ask card was deliberately removed - no rail code may paint \
             `status.ask_card_*` again"
        );
        assert!(
            !code.contains(&removed_field),
            "`AgentRow`'s removed preview field was deleted with the card it fed; a reference to \
             it here means the field (and the pty grid scrape behind it) came back"
        );
    }
}

/// Revision 6's rail corrections, in their *painted* half: real windows, real spawned processes,
/// real measured bounds. Every assertion here is about something the previous rail genuinely drew
/// differently, so each would fail against the code this issue replaced.
#[cfg(test)]
mod rail_rev6_render_tests {
    use super::*;
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn worktree_item(path: PathBuf, label: &str) -> WorktreeItem {
        WorktreeItem {
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

    /// `gpui::VisualTestContext::debug_bounds` takes a `&'static str`, and every selector here is
    /// built from a real runtime path or repo id - the same `Box::leak` idiom this file's own
    /// `click_worktree_row` and `crate::settings::render`'s `row_selector` already use, in a test
    /// binary that exits moments later.
    fn selector(name: String) -> &'static str {
        Box::leak(name.into_boxed_str())
    }

    /// A real agent whose process genuinely failed to start - a `ProcessKind::Shell` spawn with a
    /// `shell_override` naming a binary that does not exist, retagged to a real agent kind
    /// (`Agents::set_kind_for_test`, which touches only the bookkeeping) so the rail produces a
    /// row for it at all. `TerminalPane::spawn_error` is then really set, which
    /// `AdeApp::agent_status` reads as `ProcessSignal::Exited { success: false }` and
    /// `rail::status::derive_status` turns into a real [`Status::Fail`].
    fn open_with_a_failed_agent(
        cx: &mut TestAppContext,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        gpui::Entity<crate::root::AdeApp>,
        &mut gpui::VisualTestContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let id = app.agents.spawn(
                ProcessKind::Shell,
                wt.path().to_path_buf(),
                12.0,
                Some("/nonexistent/jerry-issue-289-not-a-real-binary"),
                None,
                window,
                cx,
            );
            app.agents.set_kind_for_test(id, ProcessKind::claude());
        });
        cx.run_until_parked();
        (repo, wt, app, cx)
    }

    #[gpui::test]
    fn a_failed_agent_paints_the_repo_headers_red_pair_and_no_amber_one(cx: &mut TestAppContext) {
        let (_repo, _wt, app, cx) = open_with_a_failed_agent(cx);

        let (repo_id, groups) =
            app.update(cx, |app, cx| (app.repos[0].id.0, app.build_repo_groups(cx)));
        assert_eq!(
            groups[0].failed_count(),
            1,
            "sanity check: the seeded agent really did fail to start, so this repo really does \
             hold one failed worktree"
        );
        assert_eq!(groups[0].needs_input_count(), 0, "sanity check");

        assert!(
            cx.debug_bounds(selector(format!("repo-fail-count-{repo_id}")))
                .is_some(),
            "the repo header must paint its red dot+count pair for a repo holding a failed agent"
        );
        assert!(
            cx.debug_bounds(selector(format!("repo-ask-count-{repo_id}")))
                .is_none(),
            "and must paint no amber pair at all at zero - hidden entirely, never an empty slot \
             (and never counting the failed worktree a second time in amber)"
        );
    }

    #[gpui::test]
    fn each_urgency_pair_is_an_element_only_when_its_own_count_is_nonzero(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            let repo_id = app.repos[0].id;
            for kind in [UrgencyCount::NeedsInput, UrgencyCount::Failed] {
                assert!(
                    app.render_repo_urgency_count(repo_id, kind, 0).is_none(),
                    "{kind:?} must render nothing at all at zero"
                );
                assert!(
                    app.render_repo_urgency_count(repo_id, kind, 1).is_some(),
                    "{kind:?} must render its pair as soon as it has something to report"
                );
            }
        });
    }

    #[gpui::test]
    fn the_worktree_caret_is_a_full_row_height_hit_box(cx: &mut TestAppContext) {
        let (_repo, _wt, _app, cx) = open_with_a_failed_agent(cx);

        let caret = cx
            .debug_bounds("worktree-caret-0")
            .expect("a worktree row with an agent under it must paint a caret");
        assert_eq!(
            caret.size.width,
            px(13.0),
            "the caret's hit box spans the row's whole left column"
        );
        assert_eq!(
            caret.size.height,
            px(27.0),
            "and the row's full 27px height - a caret you can only hit in an 11px square is the \
             defect §4o names"
        );
    }

    #[gpui::test]
    fn the_agent_row_is_really_indented_under_its_worktree_not_flush_with_it(
        cx: &mut TestAppContext,
    ) {
        let (_repo, wt, app, cx) = open_with_a_failed_agent(cx);

        // `open_with_a_failed_agent` opens the app (which spawns its own default shell in the
        // *repo's* own path) and then explicitly spawns the real failed agent this test cares
        // about in `wt`'s own path - the two are real, distinct tempdirs, so filtering on cwd
        // finds the right one regardless of spawn order.
        let wt_path = wt.path().to_path_buf();
        let agent_id = app.update(cx, |app, _cx| {
            app.agents
                .iter()
                .find(|a| a.cwd == wt_path)
                .expect("the real failed agent this helper seeds, in its own worktree")
                .id
        });
        let row_selector = selector(format!("agent-row-{agent_id}"));
        let content_selector = selector(format!("agent-row-content-{agent_id}"));

        let row = cx
            .debug_bounds(row_selector)
            .expect("a real agent row must paint under its worktree");
        let content = cx
            .debug_bounds(content_selector)
            .expect("the agent row's own content box must paint inside it");

        assert_eq!(
            content.origin.x - row.origin.x,
            px(14.0),
            "the content box (and the status-edge border painted on it) must start 14px in from \
             the row's own left edge - 13px of indent plus the 1px connector line - never flush \
             with x=0, which is what the worktree row's own edge uses"
        );
    }

    #[gpui::test]
    fn every_rail_list_item_spans_the_full_rail_width(cx: &mut TestAppContext) {
        let (_repo, wt, app, cx) = open_with_a_failed_agent(cx);

        let wt_path = wt.path().to_path_buf();
        let agent_id = app.update(cx, |app, _cx| {
            app.agents
                .iter()
                .find(|a| a.cwd == wt_path)
                .expect("the real failed agent this helper seeds, in its own worktree")
                .id
        });

        let list = cx
            .debug_bounds("agent-rail-list")
            .expect("the rail's own list band must paint");
        let worktree_row = cx
            .debug_bounds(selector(format!("worktree-row-0-{}", wt_path.display())))
            .expect("the worktree row must paint");
        let agent_row = cx
            .debug_bounds(selector(format!("agent-row-{agent_id}")))
            .expect("the agent row under it must paint");

        assert_eq!(
            worktree_row.size.width, list.size.width,
            "a worktree row is one gpui::list item and must span the list's real available \
             width, not shrink to fit its own label/status content"
        );
        assert_eq!(
            agent_row.size.width, list.size.width,
            "an agent row is also its own gpui::list item and must span the exact same real \
             width as the worktree row above it - a narrower agent row is what made the \
             screenshot's agent rows look misaligned/indented wrong relative to their parent"
        );
        assert_eq!(
            worktree_row.origin.x, agent_row.origin.x,
            "both rows are flush against the same list origin - the agent row's own 13px \
             indent is internal padding (proven by \
             the_agent_row_is_really_indented_under_its_worktree_not_flush_with_it above), not \
             a narrower or offset root box"
        );
    }

    #[gpui::test]
    fn the_prune_control_is_a_bin_icon_in_a_seventeen_pixel_box(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (_app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let button = cx
            .debug_bounds("rail-prune")
            .expect("the rail footer must paint its prune control");
        assert_eq!(button.size.width, px(17.0));
        assert_eq!(
            button.size.height,
            px(17.0),
            "the shared 17px icon-button hit box (`theme::band::ICON_BUTTON_HIT`), not a text \
             button's own intrinsic size"
        );

        // Regression for the screenshot-reported "icons in buttons are too big" bug: the SVG
        // glyph itself must paint at `IconSize::Control`'s real 12px optical box, not stretched
        // to fill the 17px hit box around it.
        let icon = cx
            .debug_bounds("icon-trash")
            .expect("the real vendored Phosphor `trash` SVG must paint inside it");
        assert_eq!(
            icon.size.width,
            px(12.0),
            "the trash glyph must paint at `icons::IconSize::Control`'s real optical box (12px), \
             not the 17px hit box it sits inside"
        );
        assert_eq!(icon.size.height, px(12.0));

        let left_gap = icon.origin.x - button.origin.x;
        let right_gap = (button.origin.x + button.size.width) - (icon.origin.x + icon.size.width);
        let top_gap = icon.origin.y - button.origin.y;
        let bottom_gap =
            (button.origin.y + button.size.height) - (icon.origin.y + icon.size.height);
        assert_eq!(
            left_gap, right_gap,
            "the smaller glyph must be horizontally centred in the 17px hit box"
        );
        assert_eq!(
            top_gap, bottom_gap,
            "the smaller glyph must be vertically centred in the 17px hit box"
        );
    }

    #[gpui::test]
    fn the_repo_header_sits_closer_to_its_rows_than_the_rows_sit_to_each_other(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let alpha = tempfile::tempdir().expect("tempdir alpha");
        let beta = tempfile::tempdir().expect("tempdir beta");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.worktrees = vec![
                worktree_item(alpha.path().to_path_buf(), "alpha"),
                worktree_item(beta.path().to_path_buf(), "beta"),
            ];
            // Without this the seeded list never reaches a paint pass, and `debug_bounds` below
            // would be reading the startup window's own tree.
            cx.notify();
        });
        cx.run_until_parked();

        // The rendered order is `WorktreeRow::urgency_rank`'s, not the order they were seeded in,
        // so the two row selectors are read back from the real groups this render pass built -
        // the same idiom `worktree_tab_attribution_tests::click_worktree_row` uses.
        let (repo_id, rendered_paths) = app.update(cx, |app, cx| {
            let groups = app.build_repo_groups(cx);
            (
                app.repos[0].id.0,
                groups[0]
                    .rows
                    .iter()
                    .map(|row| row.path.clone())
                    .collect::<Vec<_>>(),
            )
        });
        assert_eq!(
            rendered_paths.len(),
            2,
            "sanity check: both seeded worktrees are real rows in this repo's one group"
        );

        let header = cx
            .debug_bounds(selector(format!("repo-group-header-{repo_id}")))
            .expect("the repo band must paint");
        let first = cx
            .debug_bounds(selector(format!(
                "worktree-row-0-{}",
                rendered_paths[0].display()
            )))
            .expect("the first worktree row must paint");
        let second = cx
            .debug_bounds(selector(format!(
                "worktree-row-1-{}",
                rendered_paths[1].display()
            )))
            .expect("the second worktree row must paint");

        assert_eq!(
            header.size.height,
            px(26.0),
            "§4s: the band is 26 high with its content vertically centred, not held by \
             asymmetric padding"
        );
        let header_to_row = first.origin.y - (header.origin.y + header.size.height);
        let row_to_row = second.origin.y - (first.origin.y + first.size.height);
        assert_eq!(header_to_row, px(3.0), "3px below the band");
        assert_eq!(row_to_row, px(7.0), "7px between worktree groups");
        assert!(
            header_to_row < row_to_row,
            "the ratio is the point: a header that sits as far from its own rows as they sit \
             from each other groups nothing"
        );
    }
}

/// Proves GitHub issue #364's real fix: with many worktrees open, [`AdeApp::render_rail_list`]
/// used to build every worktree row unconditionally, on every render, regardless of scroll
/// position - which is why hovering was slow, since GPUI's own `.hover()` forces a full
/// `Window::refresh()` (and a refresh bypasses every view's own per-entity render cache) on every
/// hover-region transition. Mirrors `crate::sidebar::render::virtualization_tests` exactly, the
/// same real black-box proof this app already trusts for the file tree's own virtualized
/// `uniform_list`: absence/presence of a real painted element, not an internal call counter,
/// because that is the one thing a regression in this exact area (an eager `.children(...)` tree
/// standing in for real virtualization again) cannot fake past.
#[cfg(test)]
mod rail_virtualization_tests {
    use super::*;
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::path::Path;
    use tempfile::TempDir;

    /// Deliberately more rows than any plausible test viewport can show at 27px each, mirroring
    /// `crate::sidebar::render::virtualization_tests`' own 300-file tree fixture - "dozens to
    /// hundreds" per the live report this issue is about.
    const ROW_COUNT: usize = 120;

    fn worktree_item(path: PathBuf, label: &str) -> WorktreeItem {
        WorktreeItem {
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

    /// Seeds [`ROW_COUNT`] real, distinct worktree entries directly onto `app.worktrees` - real
    /// on-disk directories (so every row's own path is real, not merely syntactically valid), but
    /// without the real `git worktree add` process spawn each of ~120 real linked worktrees would
    /// cost: `Self::build_repo_groups`/`rail::flatten_rail_list_items`/`Self::render_rail_list`
    /// never distinguish "loaded from a real `git worktree list` porcelain scan" from "seeded
    /// directly" - both paths converge on the exact same `WorktreeItem`/`WorktreeRow`/`RepoGroup`
    /// types this file's own `rail_row_tests`/`prune_regression_tests` already seed the same way
    /// for their own synthetic fixtures. Returns every seeded path in seed order; the caller owns
    /// `keepalive` so the real `TempDir`s (and the directories they hold open) outlive the test.
    fn seed_many_worktrees(app: &mut AdeApp, keepalive: &mut Vec<TempDir>) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(ROW_COUNT);
        let mut items = Vec::with_capacity(ROW_COUNT);
        for index in 0..ROW_COUNT {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().to_path_buf();
            keepalive.push(dir);
            items.push(worktree_item(path.clone(), &format!("wt-{index:03}")));
            paths.push(path);
        }
        app.worktrees = items;
        paths
    }

    /// The real `worktree-row-{index}-{path}` `debug_selector`
    /// [`AdeApp::render_worktree_row`] paints its header under, resolved the same defensive way
    /// `crate::rail::menu_render`'s own `worktree_row_selector` test helper does: `index` is
    /// [`rail::WorktreeRow::urgency_rank`]'s real sort position, never the seed order, so this
    /// asks the live render for it rather than assuming one.
    fn worktree_row_selector(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        worktree_path: &Path,
    ) -> &'static str {
        let index = app
            .update(cx, |app, cx| app.build_repo_groups(cx))
            .iter()
            .find_map(|group| group.rows.iter().position(|row| row.path == worktree_path))
            .expect("the worktree must be a real, rendered rail row");
        Box::leak(format!("worktree-row-{index}-{}", worktree_path.display()).into_boxed_str())
    }

    #[gpui::test]
    fn a_worktree_row_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let mut keepalive = Vec::new();
        let paths = app.update(cx, |app, _cx| seed_many_worktrees(app, &mut keepalive));
        app.update(cx, |_app, cx| cx.notify());
        // Small enough that `ROW_COUNT` rows at 27px each genuinely overflow the rail's own
        // viewport many times over - the same reasoning `crate::rail::menu_render`'s own
        // `scrolling_the_rail_does_not_move_or_clip_an_open_menu` resizes for.
        cx.simulate_resize(gpui::size(px(760.0), px(400.0)));
        cx.run_until_parked();

        let first = worktree_row_selector(&app, cx, &paths[0]);
        let far_below = worktree_row_selector(&app, cx, &paths[ROW_COUNT - 1]);

        assert!(
            cx.debug_bounds(first).is_some(),
            "the first worktree row must really paint - if it doesn't, this test proves \
             nothing about virtualization, only that the rail is empty"
        );
        assert!(
            cx.debug_bounds(far_below).is_none(),
            "the {ROW_COUNT}th worktree row is far below any plausible viewport, so a real \
             virtualized list must never build it as an element at all"
        );
    }

    #[gpui::test]
    fn scrolling_the_virtualized_rail_materializes_a_row_that_was_not_painted(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let mut keepalive = Vec::new();
        let paths = app.update(cx, |app, _cx| seed_many_worktrees(app, &mut keepalive));
        app.update(cx, |_app, cx| cx.notify());
        cx.simulate_resize(gpui::size(px(760.0), px(400.0)));
        cx.run_until_parked();

        let far_path = paths[ROW_COUNT - 1].clone();
        let far_below = worktree_row_selector(&app, cx, &far_path);
        assert!(
            cx.debug_bounds(far_below).is_none(),
            "precondition: the last row must not be painted before scrolling"
        );

        // `gpui::ListState::scroll_to_reveal_item`, not a simulated wheel delta: unlike
        // `uniform_list` (every row the same measured height, so its own real maximum scroll
        // offset is known upfront), `gpui::list`'s rows are genuinely variable height and mostly
        // unmeasured this far below the fold, so a single huge wheel delta clamps against
        // whatever total height it has measured *so far* (near-zero, with only the first
        // viewport's worth of rows ever rendered) rather than the real end of a `ROW_COUNT`-row
        // list - the real, gpui-native "jump to an item by index regardless of whether its
        // height has ever been measured" API is this one.
        let target_index = app.update(cx, |app, cx| {
            let groups = app.build_repo_groups(cx);
            let items = rail::flatten_rail_list_items(&groups, |row| app.worktree_is_expanded(row));
            items
                .iter()
                .position(|item| match item {
                    rail::RailListItem::WorktreeRow {
                        group_index,
                        row_index,
                    } => groups[*group_index].rows[*row_index].path == far_path,
                    _ => false,
                })
                .expect("the far-below worktree must be a real flattened list item")
        });
        // One `scroll_to_reveal_item` call only gets as far as `gpui::ListState` can compute from
        // what it has *already measured* - real, unlike `uniform_list`'s single known row height,
        // items past whatever the viewport has ever shown are still `Unmeasured` (contributing no
        // real height to its running total yet), so revealing an item this far past the fold
        // takes the same real incremental steps a user dragging the scrollbar all the way down
        // would drive: each call measures a little further, which is what the next call's own
        // computation then has to work with. `ROW_COUNT` calls is a generous, real upper bound
        // (this fixture never needs more than a handful in practice), not a magic constant.
        for _ in 0..ROW_COUNT {
            app.update(cx, |app, cx| {
                app.rail_list_state.scroll_to_reveal_item(target_index);
                cx.notify();
            });
            cx.run_until_parked();
            if cx.debug_bounds(far_below).is_some() {
                break;
            }
        }

        assert!(
            cx.debug_bounds(far_below).is_some(),
            "scrolling to reveal the last row must really materialize it - if this fails the \
             list is not scrollable any more, which is a far worse regression than the render \
             cost this change set out to fix"
        );
    }

    #[gpui::test]
    fn hovering_a_visible_row_does_not_materialize_a_row_far_below_the_viewport(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let mut keepalive = Vec::new();
        let paths = app.update(cx, |app, _cx| seed_many_worktrees(app, &mut keepalive));
        app.update(cx, |_app, cx| cx.notify());
        cx.simulate_resize(gpui::size(px(760.0), px(400.0)));
        cx.run_until_parked();

        let first_row = worktree_row_selector(&app, cx, &paths[0]);
        let far_below = worktree_row_selector(&app, cx, &paths[ROW_COUNT - 1]);
        let first_bounds = cx
            .debug_bounds(first_row)
            .expect("the first worktree row must really paint");
        assert!(
            cx.debug_bounds(far_below).is_none(),
            "precondition: the last row must not be painted before any hover"
        );

        // A real hover-region transition: away from every row first (so entering the first row's
        // own hitbox is a genuine transition, the one GPUI's own `.hover()` reacts to by calling
        // `Window::refresh()`), then onto it.
        cx.simulate_mouse_move(gpui::point(px(1.0), px(1.0)), None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(first_bounds.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(far_below).is_none(),
            "hovering a row that is really on screen must not materialize one that is not - a \
             hover-triggered `Window::refresh()` bypassing every view's per-entity render cache \
             (see `crate::rail::state::RailListItem`'s own docs) is exactly what made the rail \
             slow to hover with many rows open before this fix, and exactly what real \
             virtualization has to stay correct under"
        );
    }
}

/// GitHub issue #336 ("Text inputs do not have selection and standard copy/paste/cut"), driven the
/// way a user drives it: real simulated clicks, drags and keystrokes into a real focused field,
/// with the real system clipboard on the other end of Ctrl/Cmd+C.
#[cfg(test)]
mod rail_filter_selection_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    fn secondary(key: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("cmd-{key}")
        } else {
            format!("ctrl-{key}")
        }
    }

    /// A focused rail filter holding `text`, with the real default key bindings live so the
    /// clipboard actions can be reached by their real keystrokes rather than dispatched directly.
    fn open_filter_with<'a>(
        cx: &'a mut TestAppContext,
        repo: &std::path::Path,
        text: &str,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.filter_focus_handle, cx);
        });
        cx.simulate_input(text);
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            text,
            "sanity check: the field really holds what was typed into it"
        );
        (app, cx)
    }

    /// A real window-space point over byte `offset` of the filter's own painted text.
    fn point_at_offset(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        offset: usize,
    ) -> gpui::Point<gpui::Pixels> {
        app.read_with(cx, |app, _| {
            let (bounds, shaped) = app
                .simple_input_layout
                .get(&gpui::SharedString::from("rail-filter-caret"))
                .expect("the filter row's own overlay must really have painted");
            gpui::point(
                bounds.origin.x + shaped.x_for_index(offset),
                bounds.origin.y + bounds.size.height / 2.0,
            )
        })
    }

    /// A point genuinely *over* the glyph at `offset` - halfway into it, so a hit test resolves to
    /// that byte rather than to whichever side of the boundary rounding happens to favour.
    fn point_inside_glyph(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        offset: usize,
    ) -> gpui::Point<gpui::Pixels> {
        let left = point_at_offset(app, cx, offset);
        let right = point_at_offset(app, cx, offset + 1);
        gpui::point((left.x + right.x) / 2.0, left.y)
    }

    #[gpui::test]
    fn shift_arrow_extends_a_real_selection_and_a_plain_arrow_collapses_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin");

        cx.simulate_keystrokes("home shift-right shift-right shift-right");
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "ori",
            "three real Shift+Right keystrokes must extend a real three-grapheme selection"
        );

        cx.simulate_keystrokes("right");
        assert!(
            app.read_with(cx, |app, _| !app.filter_query.has_selection()),
            "a plain arrow key collapses the selection rather than extending it"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.caret()),
            3,
            "...and collapses to the selection's near edge, not one grapheme past it"
        );
    }

    #[gpui::test]
    fn a_real_click_places_the_caret_and_a_drag_selects_the_range_between(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");

        let start = point_at_offset(&app, cx, 0);
        cx.simulate_mouse_down(start, gpui::MouseButton::Left, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.caret()),
            0,
            "a real click at the text's own origin must place the caret at byte 0 - before this \
             there was no click handling on these fields at all"
        );

        // ...then drag far past the field's right edge, which is exactly the gesture a
        // hover-filtered `on_mouse_move` listener could not see (see
        // `AdeApp::simple_input_overlay`'s own docs).
        let far_right = gpui::point(start.x + px(4000.0), start.y);
        cx.simulate_mouse_move(
            far_right,
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "origin/main",
            "dragging past the right edge selects to the end of the real text"
        );

        cx.simulate_mouse_up(
            far_right,
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        cx.simulate_mouse_move(start, None, gpui::Modifiers::default());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "origin/main",
            "the drag ended on mouse-up; moving the pointer afterwards must not keep selecting"
        );
    }

    #[gpui::test]
    fn a_real_double_click_selects_the_word_under_the_pointer(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");
        let inside_second_word = point_inside_glyph(&app, cx, 8);
        cx.simulate_event(gpui::MouseDownEvent {
            position: inside_second_word,
            modifiers: gpui::Modifiers::default(),
            button: gpui::MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "main",
            "GPUI's own `MouseDownEvent::click_count` is what makes this a double-click - the \
             field needs no timing of its own"
        );
    }

    #[gpui::test]
    fn a_shift_click_extends_from_the_existing_anchor(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");
        let left_edge = point_at_offset(&app, cx, 0);
        cx.simulate_mouse_down(
            left_edge,
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.simulate_mouse_up(
            left_edge,
            gpui::MouseButton::Left,
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.caret()),
            0,
            "sanity check: the first, unmodified click really moved the caret to byte 0"
        );

        let text_end = point_at_offset(&app, cx, "origin/main".len());
        cx.simulate_mouse_down(text_end, gpui::MouseButton::Left, gpui::Modifiers::shift());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "origin/main",
            "a Shift-held click extends from the caret the previous click left behind"
        );
    }

    #[gpui::test]
    fn select_all_then_copy_puts_the_real_text_on_the_real_clipboard(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");
        // Seeded with something else first, so a green result cannot come from a clipboard that
        // simply already said the right thing.
        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("stale".into()))
        });

        cx.simulate_keystrokes(&secondary("a"));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "origin/main",
            "a real mod+A keystroke must reach TextSelectAll through the real key bindings"
        );

        cx.simulate_keystrokes(&secondary("c"));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("origin/main".to_string())
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "origin/main",
            "a copy never edits the field"
        );
    }

    #[gpui::test]
    fn cut_removes_the_selection_and_paste_puts_it_back_somewhere_else(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");

        cx.simulate_keystrokes("home shift-right shift-right shift-right");
        cx.simulate_keystrokes(&secondary("x"));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "gin/main",
            "cut removes exactly the selected range"
        );
        assert_eq!(
            cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("ori".to_string())
        );

        cx.simulate_keystrokes("end");
        cx.simulate_keystrokes(&secondary("v"));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "gin/mainori",
            "paste inserts the real clipboard content at the caret"
        );

        cx.simulate_keystrokes(&secondary("z"));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "gin/main",
            "one Ctrl+Z undoes the paste and nothing more"
        );
        cx.simulate_keystrokes(&secondary("z"));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "origin/main",
            "the next one undoes the cut, selection and all"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.selected_text().to_string()),
            "ori",
            "undoing a cut restores the selection it removed, not just the text"
        );
    }

    #[gpui::test]
    fn paste_over_a_selection_replaces_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");
        cx.update(|_window, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string("upstream".into()))
        });

        cx.simulate_keystrokes(
            "home shift-right shift-right shift-right shift-right shift-right shift-right",
        );
        cx.simulate_keystrokes(&secondary("v"));
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.filter_query.as_str().to_string()),
            "upstream/main",
            "a paste with a live selection replaces that whole range"
        );
    }

    #[gpui::test]
    fn backspace_over_a_selection_deletes_the_whole_range(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");
        cx.simulate_keystrokes(&secondary("a"));
        cx.simulate_keystrokes("backspace");
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.filter_query.is_empty()),
            "one Backspace over a whole-field selection empties it, rather than removing one \
             character from the end"
        );
    }

    #[gpui::test]
    fn the_selection_is_really_measured_from_the_row_that_painted_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_filter_with(cx, repo.path(), "origin/main");
        cx.simulate_keystrokes(&secondary("a"));
        cx.run_until_parked();

        let key = gpui::SharedString::from("rail-filter-caret");
        let (start_x, end_x, width) = app.read_with(cx, |app, _| {
            let (_, shaped) = app.simple_input_layout.get(&key).expect(
                "the filter row's own overlay must really have painted and recorded its \
                         shaped line - without it there is no selection quad and no hit-testing",
            );
            let selection = app.filter_query.selection();
            (
                shaped.x_for_index(selection.start),
                shaped.x_for_index(selection.end),
                shaped.width,
            )
        });
        assert_eq!(
            start_x,
            px(0.0),
            "a whole-field selection starts at the text origin"
        );
        assert!(
            end_x > px(0.0) && end_x == width,
            "...and ends at the shaped line's own real right edge - got {end_x:?} vs {width:?}"
        );

        let hit = app.read_with(cx, |app, _| {
            let (bounds, _) = app.simple_input_layout.get(&key).expect("still painted");
            app.simple_input_offset_at(&key, gpui::point(bounds.origin.x + end_x, bounds.origin.y))
        });
        assert_eq!(
            hit,
            Some("origin/main".len()),
            "clicking at the selection's own painted right edge hit-tests to the end of the text"
        );
    }
}
