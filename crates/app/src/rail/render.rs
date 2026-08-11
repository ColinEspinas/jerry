use super::*;
use crate::root::widgets::{render_keycap_row, text_tooltip, KeycapSize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// The agent row's line-2 state word (§2.3) - deliberately distinct from [`Status::label`]
/// (`"Idle"`, used everywhere else this enum shows text, e.g. the work-surface context bar):
/// only the rail agent row uses `"paused"` for [`Status::Idle`], since that's the one place the
/// design gives it a different word ("needs input / failed / running / review ready / paused" -
/// no "idle" appears in that list at all). Changing [`Status::label`] itself to match would have
/// renamed it everywhere else in the app too.
fn agent_state_word(status: Status) -> &'static str {
    match status {
        Status::Ask => "needs input",
        Status::Fail => "failed",
        Status::Review => "review ready",
        Status::Run => "running",
        Status::Idle => "paused",
    }
}

/// The agent row's line-2 trailing text (§2.3's exact per-status table): empty for `needs
/// input` (the dot and state word are the whole message), the live activity for `running`, the
/// exit code for `failed`, the review's file count for `review ready`, and `resumable · Nh` for
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
            .map(|count| format!("{count} file{}", if count == 1 { "" } else { "s" }))
            .unwrap_or_default(),
        Status::Idle => format!("resumable \u{b7} {}", rail::format_elapsed(agent.elapsed)),
    }
}

impl AdeApp {
    /// Types (or backspaces/clears) into [`Self::filter_query`] - a small, hand-rolled text
    /// field (append/backspace only, no cursor positioning or selection) rather than
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
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        // GitHub issue #27's "solid mid-keystroke" - see `crate::palette::render::AdeApp::
        // handle_palette_key_down`'s identical reasoning.
        self.reset_caret_blink(cx);
        let changed = match keystroke.key.as_str() {
            "backspace" => self.filter_query.pop(Instant::now()),
            // A real, undoable step, not a silent loss: `Esc` clearing a typed filter is exactly
            // the case Ctrl+Z should bring back. See `crate::text_history::TextField::set`.
            "escape" => self.filter_query.clear(Instant::now()),
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => self.filter_query.push_str(text, Instant::now()),
                _ => false,
            },
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
    /// status-poll tick fills it in.
    ///
    /// A plain [`crate::work_surface::agents::ProcessKind::Shell`] never gets a row here - the
    /// rail answers "who needs me", and a shell has no turn to finish and nothing to ask. It
    /// still shows up in the tab strip (`crate::work_surface::render`, which lists everything
    /// open in the selected worktree, agents and shells alike) - this is specifically the rail's
    /// own, narrower list. A worktree whose only open pane is a shell therefore renders as an
    /// empty/idle row here, identically to a worktree with nothing open at all
    /// (`rail::build_worktree_rows` already handles that case).
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
                    .and_then(|item| item.branch.clone());

                let question_preview = if status_value == Status::Ask {
                    pane.visible_text_lines()
                        .into_iter()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                } else {
                    None
                };

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
                    question_preview,
                    exit_code: pane.exit_status().map(|status| status.exit_code()),
                    // See `AgentRow::activity`'s own docs: no real PTY-activity heuristic is
                    // wired up yet, so every row threads `None` through for now.
                    activity: None,
                    elapsed: agent.spawned_at.elapsed(),
                    review_file_count,
                }
            })
            .collect()
    }

    /// Builds one [`WorktreeRow`] per worktree, folding in every currently open agent
    /// (`crate::rail::state::build_worktree_rows`) - the single real per-render source both rail modes
    /// now build their list from (see [`Self::render_rail_list`]).
    pub(in crate::rail) fn build_worktree_rows(&self, cx: &App) -> Vec<WorktreeRow> {
        rail::build_worktree_rows(&self.build_worktree_entries(), &self.build_agent_rows(cx))
    }

    /// Builds the worktree list: every worktree `wt_core::list_worktrees` reported, including
    /// ones that failed to read - `crate::rail::worktrees::WorktreeItem`'s docs say a per-entry error
    /// is kept in the list rather than filtered out, and `Self::render_worktree_row` renders an
    /// errored entry as a visible, non-interactive row.
    ///
    /// Readable entries get their clean/merged note from [`Self::worktree_notes`] (refreshed
    /// by the same periodic task as [`Self::diff_cache`]), defaulting to "unknown yet"
    /// (`clean: None, merge: None`) for one the background snapshot hasn't reached yet.
    pub(in crate::rail) fn build_worktree_entries(&self) -> Vec<WorktreeEntry> {
        self.worktrees
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
    ///
    /// The status bar's real CPU%/memory sampling (`crate::status_bar::process_stats`) deliberately rides
    /// this same existing timer rather than spawning a second, independent polling loop -
    /// `prev_process_samples` is the one piece of state that must survive across ticks (a CPU%
    /// needs a delta between two samples), threaded through the loop body itself rather than
    /// stored on `Self`, since nothing outside this loop ever needs the raw, pre-percentage
    /// reading.
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
                        let pids: Vec<u32> = this
                            .agents
                            .iter()
                            .filter_map(|agent| agent.pane.read(cx).pid())
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
                    this.apply_review_measurements(review_measurements);
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
    ///
    /// The loop itself ticks every [`WORKTREE_WATCH_TICK`] (short - this is what gets a real
    /// watcher event in front of [`Self::load_worktrees`] well under a second, not the 5s poll
    /// interval) and, each tick, refreshes if either is true:
    /// - the watcher's [`crate::rail::worktree_watch::DirtyFlag`] is set (a real filesystem
    ///   change was observed) - after a [`WORKTREE_WATCH_SETTLE`] pause to coalesce a burst of
    ///   events from one `git worktree` invocation into a single refresh, per the issue's own
    ///   debounce requirement;
    /// - [`WORKTREE_WATCH_POLL_INTERVAL`] has elapsed since the last refresh regardless - the
    ///   backstop for changes with no filesystem-watchable signature at all (a worktree
    ///   directory deleted by hand - see [`crate::rail::worktree_watch`]'s module docs).
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
            self.prune_status = Some(format!(
                "click prune again to remove {} worktree(s)",
                candidates.len()
            ));
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
    ///
    /// Guarded by [`Self::prune_in_flight`], mirroring `Self::complete_merge_flow`/
    /// `Self::abort_merge_flow`'s `merge_op_in_flight` guard (see that field's docs for the
    /// race this closes - a second confirming click spawning a second batch into the same
    /// [`Self::_prune_task`] slot, dropping/cancelling the first).
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
        self.prune_status = Some(format!("pruning {} worktree(s)...", candidates.len()));
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
                    format!("pruned {removed} worktree(s)")
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

    /// The whole agent rail (`design_handoff_jerry_ade/README.md`'s Zone 1): header,
    /// filter row, the real scrollable agent/worktree list, and the footer - see the
    /// README's "Rail chrome" section for the exact band heights this composes
    /// (`theme::band::{RAIL_HEADER,FILTER_ROW,SURFACE_FOOTER}`).
    pub(crate) fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("agent-rail")
            // The app's real "nowhere else to put focus" fallback target - see
            // `AdeApp::rail_focus_handle`'s own docs for why the fallback lives on this
            // deliberately context-less root rather than on the filter row below it.
            .track_focus(&self.rail_focus_handle)
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_rail_header(cx))
            .child(self.render_rail_filter_row(cx))
            .when_some(self.render_worktrees_error_banner(), |el, banner| {
                el.child(banner)
            })
            .when_some(
                self.render_worktree_selection_notice_banner(cx),
                |el, banner| el.child(banner),
            )
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("agent-rail-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.rail_scroll_handle)
                            .child(self.render_rail_list(cx)),
                    )
                    .children(self.render_vertical_scrollbar(
                        "rail-scrollbar",
                        &self.rail_scroll_handle,
                        &[],
                        cx,
                    )),
            )
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

    /// Header 36 - Revision R12 §2.1: "Rail header keeps only the `+` new-session button." No
    /// section-title label - this used to say `AGENTS` (a leftover from the pre-R12 flat rail,
    /// carried through a rename to fix its vocabulary without checking it against this
    /// requirement) but the spec is explicit that the header has nothing but the button itself.
    pub(in crate::rail) fn render_rail_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rail-header")
            .flex()
            .flex_none()
            .items_center()
            .justify_end()
            .px(px(10.0))
            .h(theme::band::CHROME_HEADER)
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(self.render_new_agent_button(cx))
    }

    /// The `+` control with its real, platform-resolved `mod+N` keycap pair (`⌘N` on macOS,
    /// `Ctrl N` on Windows/Linux - `crate::keymap::resolve_combo`) - spawns a real new shell
    /// agent (see [`NewAgent`]'s docs for the judgment call on the keybinding side of
    /// this).
    pub(in crate::rail) fn render_new_agent_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("rail-new-agent")
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child(
                div()
                    .text_color(theme::text::DIM)
                    .text_size(self.ui_text_size(11.0))
                    .child("+"),
            )
            .child(render_keycap_row(
                &keymap::resolve_combo("mod+N", self.window_controls_style().is_macos()),
                KeycapSize::Standard,
            ))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.new_agent(ProcessKind::Shell, window, cx);
            }))
    }

    /// Filter row 30: `/` plus the real typed query, or the placeholder text when empty -
    /// see [`Self::handle_filter_key_down`] for the (deliberately minimal) text input.
    pub(in crate::rail) fn render_rail_filter_row(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let has_query = !self.filter_query.is_empty();

        div()
            .id("rail-filter-row")
            .track_focus(&self.filter_focus_handle)
            // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why the tag and
            // the listener both live on this exact node.
            .key_context("text-input")
            .on_action(cx.listener(Self::handle_filter_text_undo))
            .on_action(cx.listener(Self::handle_filter_text_redo))
            .on_key_down(cx.listener(Self::handle_filter_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.filter_focus_handle, cx);
            }))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    // GitHub issue #45 / live report: the caret used to be a fixed trailing
                    // child, which put it visually *after* the placeholder text whenever
                    // `filter_query` was empty. It now sits before the placeholder (the real
                    // cursor position, 0, for an empty field - matching
                    // `crate::palette::render::AdeApp::render_palette_caret`'s own empty-query
                    // placement) and after the real typed text once there is any, never
                    // appended past whatever placeholder string happens to render.
                    .when(!has_query, |el| {
                        el.child(self.render_simple_input_caret(
                            "rail-filter-caret",
                            &self.filter_focus_handle,
                        ))
                    })
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.5))
                            .text_color(if has_query {
                                theme::text::DIM
                            } else {
                                theme::text::GHOST
                            })
                            .child(if has_query {
                                self.filter_query.as_str().to_string()
                            } else {
                                "filter worktrees and agents".to_string()
                            })
                            .debug_selector(|| "rail-filter-text".to_string()),
                    )
                    .when(has_query, |el| {
                        el.child(self.render_simple_input_caret(
                            "rail-filter-caret",
                            &self.filter_focus_handle,
                        ))
                    }),
            )
    }

    /// Builds this rail's repo groups fresh from live state every render (cheap: no I/O, just
    /// field reads plus the cached [`Self::diff_cache`]/[`Self::worktree_notes`] snapshots) -
    /// see [`Self::build_worktree_rows`]'s docs. The shared foundation for
    /// [`Self::render_rail_list`] and, since it returns plain data rather than GPUI elements,
    /// this module's own tests.
    ///
    /// Each [`RepoGroup`]'s `all_rows` (what the header's `N wt`/`N worktrees waiting` counters
    /// read - [`rail::RepoGroup::waiting_count`]) is always this repo's real, complete worktree
    /// list; only `rows` (what actually renders/expands below the header) is narrowed by
    /// [`Self::filter_query`] - fixing the bug where typing into the filter box moved both
    /// numbers, not just which rows were visible (`design_handoff_jerry_ade/revision 3/
    /// REVISION-2026-07-31.md` §2.0: "a repo you have scrolled past still reports that
    /// something in it wants a human" - typing into the filter box is the same promise, just
    /// scrolled-past-in-time rather than in-space).
    ///
    /// See [`rail::group_worktrees_by_repo`]'s own docs for why every repo but
    /// [`Self::focused_repo`] still has no live data (`all_rows` empty too) today - that half is
    /// a real, separate, already-tracked data-model limitation (no per-repo worktree loading
    /// yet), not something this function papers over. Every non-focused repo gets
    /// `rows_loaded: false` (see [`rail::RepoWorktrees::rows_loaded`]'s own docs) so the render
    /// side can tell that real gap apart from a repo that was actually loaded and really has zero
    /// worktrees, rather than rendering both identically.
    pub(in crate::rail) fn build_repo_groups(&self, cx: &mut Context<Self>) -> Vec<RepoGroup> {
        let rows = self.build_worktree_rows(cx);
        let filtered: Vec<WorktreeRow> =
            rail::filter_worktree_rows(&rows, self.filter_query.as_str())
                .into_iter()
                .cloned()
                .collect();

        let repo_inputs: Vec<RepoWorktrees> = self
            .repos
            .iter()
            .map(|repo| {
                let is_focused = Some(repo.id) == self.focused_repo;
                RepoWorktrees {
                    repo_id: repo.id,
                    repo_name: repo.name.clone(),
                    all_rows: if is_focused { rows.clone() } else { Vec::new() },
                    rows: if is_focused {
                        filtered.clone()
                    } else {
                        Vec::new()
                    },
                    rows_loaded: is_focused,
                }
            })
            .collect();
        rail::group_worktrees_by_repo(repo_inputs)
    }

    /// The rail's one real structure (`design_handoff_jerry_ade/revision 3/
    /// REVISION-2026-07-31.md` §2.1: "Two levels, always: **repo group → worktree → agents**.
    /// There is **no rail mode toggle**"). See [`Self::build_repo_groups`] for how the groups
    /// themselves are built.
    pub(in crate::rail) fn render_rail_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let groups = self.build_repo_groups(cx);

        // GitHub issue #113: a repo with zero open worktrees is still a real, clickable rail
        // affordance now (`Self::render_repo_group` renders and wires up every group's header
        // regardless of `rows`/`all_rows`), so the only case left with genuinely nothing to show
        // is no repo at all - defensive rather than reachable through any real UI path today,
        // since `Self::render_rail` (this function's only caller) is itself only ever rendered
        // once `Self::focused_repo` is `Some`, which requires at least one entry in `Self::repos`.
        if groups.is_empty() {
            return self.render_rail_empty_message("no worktrees found");
        }

        let mut list = div().id("rail-repo-groups").flex().flex_col();
        for group in &groups {
            list = list.child(self.render_repo_group(group, cx));
        }
        list.into_any_element()
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

    /// One repo group (§2.0-2.1): the header (name, `N wt` count, the amber `N worktrees
    /// waiting` when non-zero, and a per-repo `+`), then either every worktree row already
    /// ranked most-urgent-first by [`rail::group_worktrees_by_repo`], or - GitHub issue #113 - a
    /// real inline message when this repo has none to show, rather than the header (and the repo
    /// itself) simply disappearing from the rail. Every repo's header renders and is clickable
    /// regardless of `rows`/`all_rows`: [`Self::checkout_repo_from_rail`] is the same real
    /// "focus/load a different repo" flow `Self::open_repo_in_current_window` (Open Folder…)
    /// already uses, so a repo with zero open worktrees is a real, reachable "focused, nothing
    /// open yet" state - not a dead end.
    ///
    /// The header's `N wt` and (via [`rail::RepoGroup::waiting_count`]) `N worktrees waiting`
    /// are read from `group.all_rows`, **not** `group.rows` - see [`Self::build_repo_groups`]'s
    /// docs for why: this repo's real, complete worktree list, unaffected by the rail's filter
    /// query or by which repo is currently focused. Only the rows actually rendered below the
    /// header (`group.rows`) may be narrower - and only that narrower list, never the header
    /// click target or the `+`, is affected by an empty vs. filtered-away distinction (see the
    /// inline message below, which does distinguish the two for its own wording).
    ///
    /// `group.rows_loaded` gates the `N wt` count itself: `false` (every non-focused repo today -
    /// see [`rail::RepoWorktrees::rows_loaded`]'s docs) renders an honest em dash instead of `0
    /// wt`, since this repo's real worktree count was never fetched and may well be nonzero - a
    /// literal `0 wt` would be a false claim about state this app hasn't actually loaded.
    pub(in crate::rail) fn render_repo_group(
        &self,
        group: &RepoGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let waiting_label = rail::waiting_count_label(group.waiting_count());
        let repo_id = group.repo_id;
        let is_focused_repo = self.focused_repo == Some(repo_id);

        let header = div()
            .id(("repo-group-header", repo_id.0))
            .debug_selector(move || format!("repo-group-header-{}", repo_id.0))
            // Padding `8 12 4` (§2.1).
            .flex()
            .items_center()
            .gap(px(6.0))
            .pt(px(8.0))
            .px(px(12.0))
            .pb(px(4.0))
            // The already-focused repo's own header isn't a real click target (see
            // `Self::checkout_repo_from_rail`'s own no-op-when-already-focused guard) - no
            // `cursor_pointer`/hover affordance for a click that would do nothing, matching
            // `render_worktree_row`'s identical `is_selected` convention just below in this same
            // file (see the comment near line 1407 for this file's established "non-actionable
            // control drops cursor_pointer/hover/on_click" rule).
            .when(!is_focused_repo, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(theme::rail::WORKTREE_HOVER_BG))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.checkout_repo_from_rail(repo_id, window, cx);
            }))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::rail::REPO_HEADER_NAME)
                    .child(group.repo_name.to_uppercase()),
            )
            .child(
                div()
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
            .child(div().flex_1())
            .when_some(waiting_label, |el, text| {
                el.child(
                    div()
                        .font(font(theme::font::SANS))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::status::ASK_CARD_FG)
                        .child(text),
                )
            })
            .child(self.render_repo_group_new_button(repo_id, cx));

        let mut group_div = div()
            .id(("repo-group", repo_id.0))
            .flex()
            .flex_col()
            .child(header);

        if group.rows.is_empty() {
            // GitHub issue #113: previously this repo's whole group (header included) was
            // dropped from the rail entirely whenever it had no rows to show - see
            // `Self::render_rail_list`'s own updated docs. A real, worded inline message now
            // takes that empty row-list's place instead, distinguishing three real cases: this
            // repo's data was never loaded (`!group.rows_loaded` - every non-focused repo today),
            // it genuinely has no open worktrees, or the filter box is hiding them - never
            // claiming "no worktrees open yet" for a repo whose data this app hasn't actually
            // fetched, which may well have several worktrees on disk.
            group_div = group_div.child(
                div()
                    .px(px(12.0))
                    .pb(px(6.0))
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOSTER)
                    .child(if !group.rows_loaded {
                        "not loaded yet \u{2013} click to open"
                    } else if group.all_rows.is_empty() {
                        "no worktrees open yet"
                    } else {
                        "no worktrees match this filter"
                    }),
            );
        } else {
            group_div = group_div.children(
                group
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(index, row)| self.render_worktree_row(row, index, cx)),
            );
        }
        group_div
    }

    /// The repo header's own `+` (GitHub issue #113) - the rail-native way to create a terminal
    /// or agent session directly in a repo, even one with zero open worktrees, without first
    /// hunting for the tab strip's identical control. Checks `repo_id` out
    /// ([`Self::checkout_repo_from_rail`] - a no-op if it's already focused) and then opens
    /// exactly the same real popover the tab strip's own `+` does
    /// ([`crate::work_surface::render::AdeApp::render_plus_menu`]), rather than reimplementing
    /// any of its five actions (New terminal, New agent, Git graph, Open file…, Next changed
    /// file) here: this button only ever decides *which repo* those actions target, never what
    /// they do once clicked. `Self::load_agent_rows` refresh mirrors
    /// `crate::work_surface::render::AdeApp::render_tab_strip_plus`'s own click handler, so the
    /// menu's "New agent" row reflects a fresh `$PATH` search here too.
    ///
    /// `cx.stop_propagation()` keeps this click from also bubbling into the header's own
    /// `on_click` right above it in the tree - both would call
    /// [`Self::checkout_repo_from_rail`] harmlessly (its own guard makes the second call a
    /// no-op), but only this handler should also open the menu, the same "inner control stops
    /// the outer row's own click" pattern `render_worktree_row`'s caret already uses.
    pub(in crate::rail) fn render_repo_group_new_button(
        &self,
        repo_id: repo::RepoId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(("repo-group-new", repo_id.0))
            .debug_selector(move || format!("repo-group-new-{}", repo_id.0))
            .flex_none()
            .w(px(14.0))
            .h(px(14.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .rounded(theme::radius::CHIP)
            .text_color(theme::text::DIM)
            .text_size(self.ui_text_size(11.0))
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child("+")
            // Captures this button's own painted bounds into `Self::rail_plus_button_bounds`
            // every render - the same `gpui::canvas` idiom `Self::plus_button_bounds` uses for
            // the tab strip's `+` (`crate::work_surface::render::AdeApp::render_tab_strip_plus`),
            // keyed by `repo_id` since more than one of these paints per frame. Lets
            // `crate::work_surface::render::AdeApp::render_plus_menu` anchor the popover to
            // *this* button rather than the tab strip's when this is the one that opened it - see
            // `Self::plus_menu_repo_anchor`'s own docs.
            .child({
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.rail_plus_button_bounds.insert(repo_id, bounds);
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.checkout_repo_from_rail(repo_id, window, cx);
                // GitHub issue #176 - see `AdeApp::close_menu_surfaces_except`. Runs before the
                // two assignments below, since the sweep clears `plus_menu_repo_anchor`.
                let _ = this.close_menu_surfaces_except(Some(menus::MenuSurface::Plus));
                this.plus_menu_open = true;
                this.plus_menu_repo_anchor = Some(repo_id);
                this.load_agent_rows(cx);
                cx.notify();
            }))
    }

    /// Whether `row`'s agent rows are currently shown - an explicit per-worktree override in
    /// [`Self::rail_collapse_overrides`] if the caret has ever been clicked for this path,
    /// otherwise the real default (§2.2: "Worktrees whose most urgent agent is idle start
    /// collapsed"). An agent-less row has no caret at all, so this is only ever consulted
    /// (via [`Self::render_worktree_row`]) when `row.agents` is non-empty.
    ///
    /// GitHub issue #112 (live follow-up report): the worktree currently selected
    /// ([`Self::active_agent_cwd`] - the same real comparison [`Self::render_worktree_row`]'s own
    /// `is_selected` uses) is exempt from the idle-collapse default, absent an explicit override.
    /// Without this, a worktree the user is actively switching terminals within could silently
    /// collapse out from under them the moment its most urgent agent's status crossed into
    /// `Idle` (an ordinary, real occurrence - a shell sitting at its prompt between commands),
    /// replacing both visible terminal rows with a single collapsed summary row and reading, from
    /// the report, as the terminals having "merged into one" - no data was ever lost (the tab
    /// strip stays untouched by this purely rail-side collapse), but the row the user was looking
    /// at should never vanish out from under active use. An explicit caret click still always
    /// wins over this, same as it already does over the plain idle default - a user who
    /// deliberately collapses the active worktree gets to keep it collapsed.
    pub(in crate::rail) fn worktree_is_expanded(&self, row: &WorktreeRow) -> bool {
        match self.rail_collapse_overrides.get(&row.path) {
            Some(expanded) => *expanded,
            None => self.active_agent_cwd() == row.path || row.aggregate_status() != Status::Idle,
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

    /// One worktree row (§2.2: 27 high, padding `0 10 0 6`, gap 6) plus, when expanded, its
    /// agent rows (§2.3) - the rail's real "worktree owns N agents" structure. `index` (unique
    /// within its repo group) disambiguates element ids for the real degenerate case
    /// `crate::rail::worktrees::WorktreeItem`'s docs call out: more than one unreadable worktree
    /// entry shares the same (empty) `path`, which alone would collide.
    ///
    /// Clicking the row selects this worktree (`Self::select_worktree_by_path`), restoring
    /// whatever tab it was left on (§2.3: "Clicking a worktree header restores whatever tab it
    /// was left on") - switching tabs within it happens in the centre pane's own tab strip, or
    /// by clicking one of the agent rows below directly (see [`Self::render_agent_row`]).
    pub(in crate::rail) fn render_worktree_row(
        &self,
        row: &WorktreeRow,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = format!("worktree-row-{index}-{}", row.path.display());

        if let Some(error) = &row.error {
            // A real error row, per `crate::rail::worktrees::WorktreeItem`'s documented intent:
            // visible, not silently dropped - and deliberately not clickable (an errored
            // entry has no usable, real path to select into).
            return div()
                .id(id)
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

        let is_selected = self.active_agent_cwd() == row.path;
        let has_agents = !row.agents.is_empty();
        let is_expanded = has_agents && self.worktree_is_expanded(row);
        let status = row.aggregate_status();

        // 2px left edge = the colour of the worktree's most urgent agent - bare/prunable get
        // their own dedicated colours (§2.2).
        let edge_color: gpui::Rgba = if has_agents {
            status.color()
        } else if row.note.is_prunable() {
            theme::rail::PRUNABLE_EDGE.into()
        } else {
            theme::status::IDLE_BG.into()
        };

        // `#dde2e7` active / `#c2c7cc` with agents / `#8b9197` bare (§2.2).
        let branch_color: gpui::Rgba = if is_selected {
            theme::text::SELECTED.into()
        } else if has_agents {
            theme::text::STRONG.into()
        } else {
            theme::text::DIM.into()
        };

        let caret = if has_agents {
            let worktree_path = row.path.clone();
            Some(
                div()
                    .id(("worktree-caret", index as u64))
                    .flex_none()
                    .w(px(11.0))
                    .h(px(11.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(8.0))
                    .text_color(theme::text::FAINT)
                    // GitHub issue #128 - same lightweight text-only hover
                    // `Self::render_status_zoom_value` uses for an equally small, box-free
                    // clickable glyph.
                    .hover(|el| el.text_color(theme::text::SELECTED))
                    .child(if is_expanded { "\u{25be}" } else { "\u{25b8}" })
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.toggle_worktree_collapsed(worktree_path.clone(), is_expanded, cx);
                    })),
            )
        } else {
            None
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
            let (add, del) = row.diff_totals();
            if add > 0 || del > 0 {
                trailing = trailing.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("+{add} \u{2212}{del}")),
                );
            }
        }

        let path = row.path.clone();
        let header = div()
            .id(id)
            .cursor_pointer()
            .flex()
            .items_center()
            .h(px(27.0))
            .pl(px(6.0))
            .pr(px(10.0))
            .gap(px(6.0))
            .border_l(px(2.0))
            .border_color(edge_color)
            .when(is_selected, |el| el.bg(theme::rail::WORKTREE_ACTIVE_BG))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(theme::rail::WORKTREE_HOVER_BG))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_worktree_by_path(&path, window, cx);
            }))
            .children(caret)
            .child(branch_div)
            .when(!has_agents, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::text::GHOSTER)
                        .child(format!("\u{b7} {}", row.note.label())),
                )
            })
            .child(div().flex_1().min_w(px(2.0)))
            .child(trailing);

        let mut container = div()
            .id(("worktree-group", index as u64))
            .flex()
            .flex_col()
            .child(header);
        if is_expanded {
            for agent in &row.agents {
                container = container.child(self.render_agent_row(agent, cx));
            }
        }

        // GitHub issue #12's "locked worktrees are visually marked, with the lock reason
        // surfaced (tooltip is fine)" - `row.note.is_locked` alone (already threaded through
        // `build_worktree_entries` from `WorktreeItem::is_locked`) is what drives the `·
        // locked`/`locked` text `WorktreeNote::label` already renders in the stat column above;
        // this adds the *reason* as a tooltip on the whole row. Looked up from `self.worktrees`
        // by path rather than threaded onto `WorktreeRow` itself - `WorktreeNote` is shared with
        // the periodic status-poll snapshot (`crate::rail::state::compute_status_snapshot`) and
        // already has a lot of call sites; a worktree list is always small, so a linear lookup
        // here per row per render is real but negligible cost next to everything else this
        // function already computes.
        if row.note.is_locked {
            let lock_reason = self
                .worktrees
                .iter()
                .find(|item| item.path == row.path)
                .and_then(|item| item.lock_reason.clone());
            let tooltip_text = match lock_reason {
                Some(reason) => format!("Locked: {reason}"),
                None => "Locked".to_string(),
            };
            container = container.tooltip(text_tooltip(tooltip_text));
        }

        // The most urgently-waiting open agent's own question preview, if any - matches the
        // old per-agent row's card exactly, just picked from among this worktree's several
        // possible tabs rather than always having exactly one to show.
        let question_preview = row
            .agents
            .iter()
            .filter(|agent| agent.status == Status::Ask)
            .find_map(|agent| agent.question_preview.as_ref());
        if let Some(preview) = question_preview {
            container = container.child(
                div()
                    .mt(px(4.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::status::ASK_CARD_BG)
                    .border_l(px(2.0))
                    .border_color(theme::status::ASK_CARD_EDGE)
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(theme::status::ASK_CARD_FG)
                    .child(preview.clone()),
            );
        }

        container.into_any_element()
    }

    /// One agent row (§2.3): indented 13, a 1px spine (2px and status-coloured when this is the
    /// globally active agent - `Self::agents::active_id`), exactly two lines - chip/title/
    /// elapsed, then status dot/state word/trailing text/model. Clicking it selects this
    /// agent's tab *and* its worktree (`Self::select_agent` - already does both: it's the
    /// same real entry point the palette/tab-strip use to jump straight to one agent).
    pub(in crate::rail) fn render_agent_row(
        &self,
        agent: &AgentRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.agents.active_id() == Some(agent.id);
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
            .cursor_pointer()
            .flex()
            .flex_col()
            .pl(px(13.0))
            .pr(px(10.0))
            .py(px(4.0))
            .gap(px(2.0))
            .border_l(if is_selected { px(2.0) } else { px(1.0) })
            .border_color(if is_selected {
                status.color()
            } else {
                theme::border::ZONE.into()
            })
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(theme::rail::WORKTREE_HOVER_BG))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_agent(id, window, cx);
            }))
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
                            .text_size(self.ui_text_size(11.5))
                            .text_color(if is_selected {
                                theme::text::SELECTED
                            } else {
                                theme::text::BODY
                            })
                            .child(agent.title.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::GHOST)
                            .child(rail::format_elapsed(agent.elapsed)),
                    ),
            )
            .child(
                // Line 2, indented 21 to the text column (chip width 15 + gap 6): status dot ·
                // state word · trailing text · model.
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
            )
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

    /// Footer 28: real aggregate stats (`N worktrees · disk usage`) plus the real `prune`
    /// action.
    pub(in crate::rail) fn render_rail_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Includes error'd entries - the count should match what `wt_core::list_worktrees`
        // reported, problems included, not silently shrink.
        let worktree_count = self.worktrees.len();
        let disk_label = self.disk_usage_label();
        let prunable_count = self.prunable_worktree_paths().len();
        let prune_label = if self.prune_in_flight {
            "pruning\u{2026}".to_string()
        } else if self.prune_confirm_armed {
            format!("confirm prune ({prunable_count})?")
        } else {
            format!("prune ({prunable_count})")
        };

        let prune_button = div()
            .id("rail-prune")
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(10.0));
        // Mirrors `Self::render_merge_flow_footer`'s `in_flight` gating: while a prune batch
        // is running, this button drops `cursor_pointer`/hover/`on_click` entirely rather
        // than staying enabled-looking and inviting a click `Self::execute_prune`'s guard
        // would silently swallow.
        let prune_button = if self.prune_in_flight {
            prune_button
                .cursor_default()
                .text_color(theme::text::DISABLED)
                .child(prune_label)
        } else {
            prune_button
                .cursor_pointer()
                .text_color(if prunable_count > 0 {
                    theme::button::DANGER_FG
                } else {
                    theme::text::DISABLED
                })
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .child(prune_label)
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.request_prune(cx);
                }))
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
                    .unwrap_or_else(|| format!("{worktree_count} worktrees \u{b7} {disk_label}"));
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

    /// Deliberately discriminating, not just end-state-checking: arming/confirming twice
    /// against the *same* candidate would pass whether or not `Self::prune_in_flight` exists
    /// (a double-spawned batch removing one worktree twice just fails harmlessly the second
    /// time). Instead this seeds a *second*, independent prunable worktree only after the
    /// first batch is already in flight, so it can only be removed by a genuine second batch
    /// spawning - if the guard is broken, `second` gets removed too.
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

        // Click 1: arm.
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

        // Click 3: re-arm - `second` is now a real prune candidate too.
        app.update(cx, |app, cx| app.request_prune(cx));
        assert!(app.read_with(cx, |app, _| app.prune_confirm_armed));

        // Click 4: confirm again, while the first batch is still genuinely in flight. If the
        // guard works, `execute_prune` returns having done nothing - no second batch, no
        // second candidate list, `second` is never touched.
        app.update(cx, |app, cx| app.request_prune(cx));

        // Now let whichever batch(es) actually got spawned run to completion.
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

    /// §2.2: "Worktrees whose most urgent agent is idle start collapsed" - proven here against
    /// a running agent (never collapsed by default) and a real idle one (collapsed by default),
    /// through `Self::worktree_is_expanded`, the single real place that default lives.
    ///
    /// The running case is a synthetic `AgentRow` rather than a real spawned agent: a plain
    /// shell no longer produces any rail row at all (see `Self::build_agent_rows`'s own docs),
    /// so it can't stand in for "a running agent" here any more, and reaching for a real
    /// `claude`/`codex` spawn just to get one `Status::Run` row would trade a fast, deterministic
    /// test for a slow one that also depends on those CLIs being installed - this test is about
    /// `worktree_is_expanded`'s idle-rooted default, not about spawning.
    ///
    /// Selects a *second*, unrelated worktree rather than `wt` itself (GitHub issue #112 live
    /// follow-up: the currently selected worktree is now exempt from this default - see
    /// `Self::worktree_is_expanded`'s own docs) so this test keeps proving the plain idle-rooted
    /// rule in isolation; `the_selected_worktree_never_idle_collapses_by_default` below covers
    /// the exemption itself.
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
            question_preview: None,
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

    /// **The regression test for this fix.** The rail answers "who needs me", and a plain shell
    /// never needs anyone - so a worktree whose only open pane is a shell must produce zero rail
    /// rows, the same as a worktree with nothing open at all. The tab strip
    /// (`crate::work_surface::render`) is the real place a shell tab shows up; this test proves
    /// the rail and the tab strip are allowed to disagree about that on purpose.
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

    /// GitHub issue #112 (live follow-up report): a worktree the user is actively switching
    /// terminals within - the one [`crate::root::AdeApp::active_agent_cwd`] currently reports -
    /// must never auto-collapse just because its most urgent agent's status happens to cross into
    /// `Idle` (an ordinary occurrence - a shell sitting at its prompt between commands). Before
    /// this exemption, that real, wall-clock-driven Idle transition would silently collapse the
    /// row out from under active use, replacing both visible terminal rows with a single
    /// collapsed summary line and reading, from the report, as the terminals having "merged into
    /// one" - even though nothing was ever closed (the tab strip stayed untouched the whole
    /// time). An explicit caret click still wins over the exemption, same as it already wins over
    /// the plain idle default.
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
            app.read_with(cx, |app, _| app.active_agent_cwd()),
            wt.path(),
            "premise: `wt` really is the currently selected worktree"
        );

        // Same idle-forcing technique as the sibling test above.
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

    /// The caret's real click behaviour: flips whatever the current expanded state is into an
    /// explicit, remembered override - and a second toggle flips it right back, proving this is
    /// a real per-worktree memory, not a write-only flag.
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

    /// §2.3: "Clicking an agent selects its worktree **and** raises that agent's tab." -
    /// `Self::render_agent_row`'s own click handler calls exactly `Self::select_agent`, so this
    /// exercises that same real call: starting from worktree A selected/focused, selecting a
    /// agent that lives in worktree B must move the rail's selection to B *and* make that
    /// exact agent the active tab - not just one half of the pair.
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
                window,
                cx,
            );
            app.select_worktree(0, window, cx);
            app.agents.spawn(
                ProcessKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
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

    /// A broader smoke test through the real repo-group → worktree-row → agent-row pipeline
    /// (`Self::render_rail_list`) with a bare worktree (no caret/agents) alongside a busy one
    /// with two agent rows - the same trees `AdeApp::render` composes every frame. Only asserts
    /// it completes without panicking; the exact pixel spec is covered by the pure per-field
    /// logic tests in `crate::rail::state` and this module's other tests.
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
                window,
                cx,
            );
            app.agents.spawn(
                ProcessKind::codex(),
                busy_wt.path().to_path_buf(),
                12.0,
                None,
                window,
                cx,
            );
        });

        app.update(cx, |app, cx| {
            let _ = app.render_rail_list(cx);
        });
    }

    /// The real bug the coordinator's audit found: typing into the rail's filter box must
    /// change only which rows a repo group *renders*, never the header's `N wt` count
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.1) - that number must
    /// keep reporting the repo's real, complete worktree list, exactly like `RepoGroup::
    /// waiting_count` (proven independently, against hand-built rows, by `crate::rail::state`'s
    /// own `repo_group_header_counts_read_the_real_worktree_list_not_the_displayed_rows`) does.
    /// This test drives the same guarantee through the real, live `AdeApp`: two real worktrees,
    /// a filter query that matches only one of them.
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

        // Type a filter query that matches only "alpha", not "beta".
        app.update(cx, |app, _cx| {
            app.filter_query
                .push_str("alpha", std::time::Instant::now());
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
}

/// GitHub issue #113: "no way to select an empty repo (one with no worktrees/sessions open yet)
/// from the rail." Real click-through coverage against the live `AdeApp`/window, mirroring
/// `crate::code_surface::render`'s own `cx.simulate_click`-against-`debug_bounds` technique -
/// not just calling `Self::checkout_repo_from_rail` directly, since the bug this closes was two
/// real gaps in the *render* side (no `on_click` on the header at all, and the whole group
/// vanishing when it had no rows) that a handler-level test alone wouldn't catch a regression of.
#[cfg(test)]
mod repo_checkout_tests {
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    /// Repo B is added (`Self::add_repo`) but never focused - the exact "known to the rail, zero
    /// open worktrees/agents" state the issue describes: `Self::build_repo_groups` only
    /// populates real row data for `Self::focused_repo` (see that function's own docs), so repo
    /// B's group renders with both `rows` and `all_rows` empty. Before this change,
    /// `Self::render_rail_list` dropped that whole group (header included) from the rail
    /// entirely; now the header must still paint, with a real, working click target.
    #[gpui::test]
    fn clicking_an_empty_repos_header_checks_it_out(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir a");
        let repo_b = tempfile::tempdir().expect("tempdir b");
        std::fs::write(repo_b.path().join("b.txt"), "b\n").expect("write");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));
        assert_eq!(
            groups.len(),
            2,
            "sanity check: both repos must produce a group at all"
        );
        let repo_b_group = groups
            .iter()
            .find(|group| group.repo_id == repo_b_id)
            .expect("repo B's group must render despite having zero rows - GitHub issue #113");
        assert!(
            repo_b_group.rows.is_empty() && repo_b_group.all_rows.is_empty(),
            "sanity check: repo B genuinely has no live worktree data loaded yet"
        );
        assert_ne!(
            app.read_with(cx, |app, _| app.focused_repo_path()),
            repo_b.path(),
            "sanity check: repo B is not the focused repo before the click"
        );

        // A real click on repo B's header's own painted bounds, not a direct method call - see
        // this module's own docs for why the render side matters here.
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
                repo_b.path(),
                "clicking repo B's header must check it out - focus its repo"
            );
            assert_eq!(
                app.file_tree_root,
                repo_b.path(),
                "checking out repo B must really load its own file tree, not just flip a focus id"
            );
        });
    }

    /// A checker audit of the fix above (GitHub issue #113) found a real "no fake functionality"
    /// violation: repo B's group renders with empty `rows`/`all_rows` not because it really has
    /// zero worktrees, but because its data was simply never loaded (single-repo-scoped rail -
    /// see `rail::group_worktrees_by_repo`'s own docs). Before `RepoGroup::rows_loaded` existed,
    /// the render side had no way to tell that apart from a real zero-worktree repo, so it showed
    /// a literal "0 wt" and "no worktrees open yet" for repo B - both false claims about state
    /// this app hasn't actually fetched. This proves the fix: the focused repo (real data) must
    /// report `rows_loaded: true`, and every other repo (unfetched) must report `false`.
    #[gpui::test]
    fn build_repo_groups_marks_only_the_focused_repos_data_as_really_loaded(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir a");
        let repo_b = tempfile::tempdir().expect("tempdir b");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_a_id = app.read_with(cx, |app, _| {
            app.focused_repo()
                .expect("sanity check: repo A is focused")
                .id
        });
        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        let groups = app.update(cx, |app, cx| app.build_repo_groups(cx));

        let group_a = groups
            .iter()
            .find(|g| g.repo_id == repo_a_id)
            .expect("repo A's group must exist");
        assert!(
            group_a.rows_loaded,
            "the focused repo's own data really was loaded - rows_loaded must be true"
        );

        let group_b = groups
            .iter()
            .find(|g| g.repo_id == repo_b_id)
            .expect("repo B's group must exist");
        assert!(
            !group_b.rows_loaded,
            "repo B's data was never fetched (it isn't the focused repo) - rows_loaded must be \
             false, not indistinguishable from a repo that was loaded and really has zero \
             worktrees"
        );
        assert!(
            group_b.all_rows.is_empty() && group_b.rows.is_empty(),
            "sanity check: repo B's rows are empty only because nothing was ever loaded for it"
        );
    }

    /// A second click on the already-focused repo's own header must be a real no-op (matching
    /// `Self::checkout_repo_from_rail`'s own guard) - proven by arming some real per-repo UI
    /// state and confirming it survives the click, the same "did this actually reset anything"
    /// shape `open_repo_in_current_window_clears_stale_ui_state_from_the_previous_repo`
    /// (`crate::root::mod`) uses for the real switch case.
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
            .expect("the focused repo's own header must still paint (and still be clickable)");
        cx.simulate_click(header_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app.commit_menu_open),
            "re-clicking the already-focused repo's own header must not reset its live UI state"
        );
    }

    /// The rail's own per-repo `+` (GitHub issue #113's second half: "let the user create a
    /// terminal / agent session ... from the rail") must check the target repo out first and
    /// then open the exact same real popover the tab strip's own `+` uses
    /// (`crate::work_surface::render::AdeApp::render_plus_menu`) - not a second, reimplemented
    /// spawn path. Driven end to end: click repo B's `+`, then click the real "New terminal" row
    /// that popover renders, and confirm the spawned terminal's `cwd` is really repo B's.
    #[gpui::test]
    fn the_repo_headers_plus_button_opens_the_real_plus_menu_targeting_that_repo(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir a");
        let repo_b = tempfile::tempdir().expect("tempdir b");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.agents
                    .iter_for_cwd(repo_b.path().to_path_buf())
                    .next()
                    .is_none(),
                "sanity check: repo B is a genuinely empty repo - no agents open in it yet"
            );
        });

        let new_button_selector: &'static str =
            Box::leak(format!("repo-group-new-{}", repo_b_id.0).into_boxed_str());
        let new_button_bounds = cx
            .debug_bounds(new_button_selector)
            .expect("repo B's own + must have painted");
        cx.simulate_click(new_button_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo_b.path(),
                "the rail's + must check repo B out before offering to spawn into it"
            );
            assert!(
                app.plus_menu_open,
                "the rail's + must open the real tab-strip plus menu, not spawn silently"
            );
        });

        let new_terminal_bounds = cx.debug_bounds("dropdown-menu-row-New terminal").expect(
            "the real plus menu's own New terminal row must have painted - proving this \
             reuses `render_plus_menu` rather than a reimplemented popover",
        );
        cx.simulate_click(new_terminal_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                !app.plus_menu_open,
                "picking a row must close the menu, same as every other plus-menu click"
            );
            let agent = app
                .agents
                .active()
                .expect("New terminal must really spawn an agent and make it active");
            assert_eq!(
                agent.cwd,
                repo_b.path(),
                "the spawned terminal must run in repo B - the repo the rail's own + checked \
                 out - not wherever was focused before the click"
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

    /// GitHub issue #45's own title, taken literally: the caret must actually *blink* (not just
    /// exist) once this field is focused - proving `filter_focus_handle`'s real wiring into
    /// `crate::root::caret_blink`'s shared loop by advancing the real (simulated) clock past one
    /// full interval and observing `caret_blink_visible` really flip, the same live-loop proof
    /// `crate::code_surface::editing`'s own rehighlight-debounce tests use for their timers.
    /// `cx.simulate_input` (not a bare `window.focus`) is what actually forces the window to
    /// redraw and diff its own focus path in this test harness - the real trigger
    /// `on_focus`/`on_blur` listeners fire from (see `gpui::Window::focus`'s own deferred-effect
    /// doc comment) - matching how a real user always focuses a field by clicking or tabbing
    /// into it and then typing, never focus with no further interaction.
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
/// actually switches between the app's default letter chip and a real pack SVG, rather than
/// just trusting the render code's own claim to do so.
#[cfg(test)]
mod agent_chip_icon_pack_tests {
    use crate::rail::worktrees::WorktreeItem;
    use crate::root::focus::palette_focus_tests;
    use crate::work_surface::agents::ProcessKind;
    use gpui::TestAppContext;

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
    ///
    /// Real `AgentKind::Claude`, not `ProcessKind::Shell`: the rail never renders a row for a
    /// plain shell at all (`AdeApp::build_agent_rows`'s own docs - a shell has nothing for the
    /// rail to triage), so it can no longer stand in for "an agent row" here. The icon-chip
    /// logic under test (`AdeApp::render_agent_chip_icon`) doesn't care which real kind it's
    /// given - it's exercised identically either way.
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
            cx.debug_bounds("agent-chip-icon-pack-svg").is_none(),
            "with no icon pack configured, no pack SVG element must paint at all"
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
            cx.debug_bounds("agent-chip-icon-pack-svg").is_some(),
            "once a real pack directory with a matching shell.svg is configured, the rail row \
             must really switch to painting the pack's own SVG element"
        );
        assert!(
            cx.debug_bounds("agent-chip-icon-default").is_none(),
            "the default letter chip must not also paint once the pack icon takes over - \
             exactly one of the two must be showing, never both at once"
        );
    }
}
