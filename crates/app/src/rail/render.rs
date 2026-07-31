use super::*;
use crate::root::widgets::{render_keycap_row, text_tooltip, KeycapSize};
use std::path::Path;

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
fn agent_trailing_text(session: &SessionRow) -> String {
    match session.status {
        Status::Ask => String::new(),
        Status::Run => session.activity.clone().unwrap_or_default(),
        Status::Fail => session
            .exit_code
            .map(|code| format!("exit {code}"))
            .unwrap_or_default(),
        Status::Review => session
            .review_file_count
            .map(|count| format!("{count} file{}", if count == 1 { "" } else { "s" }))
            .unwrap_or_default(),
        Status::Idle => format!("resumable \u{b7} {}", rail::format_elapsed(session.elapsed)),
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

    /// Builds the rail's per-session rows from live state: each session's `TerminalPane`
    /// (process signal, question preview), the matching worktree's branch name, and the diff
    /// summary from [`Self::diff_cache`] (refreshed by the periodic task started in
    /// `Self::new`). A session with no diff data yet simply shows `0`/`0` until the next
    /// status-poll tick fills it in.
    pub(crate) fn build_session_rows(&self, cx: &App) -> Vec<SessionRow> {
        self.sessions
            .iter()
            .map(|session| {
                let status_value = self.session_status(session, cx);
                let pane = session.pane.read(cx);
                let diff = self.diff_cache.get(&session.cwd).copied();

                let branch = self
                    .worktrees
                    .iter()
                    .find(|item| item.path == session.cwd)
                    .and_then(|item| item.branch.clone());

                let question_preview = if status_value == Status::Ask {
                    pane.visible_text_lines()
                        .into_iter()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                } else {
                    None
                };

                let title = match session.cwd.file_name() {
                    Some(name) => name.to_string_lossy().into_owned(),
                    None => session.cwd.display().to_string(),
                };

                // See `SessionRow::review_file_count`'s own docs: `Self::file_authorship` is
                // scoped to whichever single diff is currently loaded in Zone 3
                // (`Self::diff_root`), not tracked per-worktree yet - so a review-ready session
                // outside that one worktree has no real data to report and stays `None` rather
                // than a fabricated count.
                let review_file_count = if status_value == Status::Review
                    && session.cwd == self.diff_root
                {
                    self.current_diff().map(|diff| {
                        self.file_authorship
                            .file_count_for(session.id, diff.files.iter().map(|f| f.path.as_path()))
                    })
                } else {
                    None
                };

                SessionRow {
                    id: session.id,
                    kind: session.kind,
                    title,
                    cwd: session.cwd.clone(),
                    status: status_value,
                    branch,
                    add: diff.map(|summary| summary.add).unwrap_or(0),
                    del: diff.map(|summary| summary.del).unwrap_or(0),
                    question_preview,
                    exit_code: pane.exit_status().map(|status| status.exit_code()),
                    // See `SessionRow::activity`'s own docs: no real PTY-activity heuristic is
                    // wired up yet, so every row threads `None` through for now.
                    activity: None,
                    elapsed: session.spawned_at.elapsed(),
                    review_file_count,
                }
            })
            .collect()
    }

    /// Builds one [`WorktreeRow`] per worktree, folding in every currently open session
    /// (`crate::rail::state::build_worktree_rows`) - the single real per-render source both rail modes
    /// now build their list from (see [`Self::render_rail_list`]).
    pub(in crate::rail) fn build_worktree_rows(&self, cx: &App) -> Vec<WorktreeRow> {
        rail::build_worktree_rows(&self.build_worktree_entries(), &self.build_session_rows(cx))
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
    /// docs). Every tick: snapshots the current worktree paths, open sessions' cwds, and open
    /// sessions' real pids on the foreground thread (cheap, no I/O), computes a
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

                let Ok((worktrees, diff_paths, pids)) = this.update(cx, |this, cx| {
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
                    let diff_paths: Vec<PathBuf> = this
                        .sessions
                        .iter()
                        .map(|session| session.cwd.clone())
                        .collect();
                    let pids: Vec<u32> = this
                        .sessions
                        .iter()
                        .filter_map(|session| session.pane.read(cx).pid())
                        .collect();
                    (worktrees, diff_paths, pids)
                }) else {
                    break;
                };

                let (snapshot, process_samples, next_prev) = cx
                    .background_executor()
                    .spawn(async move {
                        let snapshot = rail::compute_status_snapshot(&worktrees, &diff_paths);
                        let (process_samples, next_prev) =
                            process_stats::sample_processes(&pids, prev_process_samples);
                        (snapshot, process_samples, next_prev)
                    })
                    .await;
                prev_process_samples = next_prev;

                let updated = this.update(cx, |this, cx| {
                    this.diff_cache = snapshot.diffs;
                    this.worktree_notes = snapshot.worktree_notes;
                    this.ahead_behind_cache = snapshot.ahead_behind;
                    this.process_stats = process_samples;
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        });
        self._status_poll_task = Some(task);
    }

    /// The prune candidate list: every worktree that is a prune candidate on its own merits
    /// ([`rail::is_prunable`]) **and** has no live session running with its cwd inside it -
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
        let live_session_cwds: HashSet<PathBuf> = self
            .sessions
            .iter()
            .map(|session| session.cwd.clone())
            .collect();
        rail::prunable_worktree_paths(&worktree_paths, &self.worktree_notes, &live_session_cwds)
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

    /// The whole session rail (`design_handoff_jerry_ade/README.md`'s Zone 1): header,
    /// filter row, the real scrollable session/worktree list, and the footer - see the
    /// README's "Rail chrome" section for the exact band heights this composes
    /// (`theme::band::{RAIL_HEADER,FILTER_ROW,SURFACE_FOOTER}`).
    pub(crate) fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("session-rail")
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
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("session-rail-list")
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

    /// A visible error banner for [`Self::worktrees_error`] (`wt_core::list_worktrees`
    /// failing outright, e.g. a corrupt repository) - shown as a standing banner rather than
    /// replacing the whole session list, so already-open sessions stay usable even when the
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

    /// Header 36 (`Sessions` label, grouping toggle, `+`/⌘N) - README's "Rail chrome".
    pub(in crate::rail) fn render_rail_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rail-header")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .h(theme::band::RAIL_HEADER)
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::FAINT)
                    .child("SESSIONS"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(self.render_new_session_button(cx)),
            )
    }

    /// The `+` control with its real, platform-resolved `mod+N` keycap pair (`⌘N` on macOS,
    /// `Ctrl N` on Windows/Linux - `crate::keymap::resolve_combo`) - spawns a real new shell
    /// session (see [`NewSession`]'s docs for the judgment call on the keybinding side of
    /// this).
    pub(in crate::rail) fn render_new_session_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("rail-new-session")
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
                this.new_session(SessionKind::Shell, window, cx);
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
                                "filter sessions".to_string()
                            }),
                    )
                    .child(self.render_simple_input_caret()),
            )
    }

    /// The rail's one real structure (`design_handoff_jerry_ade/revision 3/
    /// REVISION-2026-07-31.md` §2.1: "Two levels, always: **repo group → worktree → agents**.
    /// There is **no rail mode toggle**"). Builds [`rail::WorktreeRow`]s fresh from live state
    /// every render (cheap: no I/O, just field reads plus the cached
    /// [`Self::diff_cache`]/[`Self::worktree_notes`] snapshots) - see [`Self::build_worktree_rows`]'s
    /// docs - filters them, then groups the result by repo (see [`rail::group_worktrees_by_repo`]'s
    /// own docs for why every repo but [`Self::focused_repo`] renders with zero rows today).
    pub(in crate::rail) fn render_rail_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let rows = self.build_worktree_rows(cx);
        let filtered: Vec<WorktreeRow> =
            rail::filter_worktree_rows(&rows, self.filter_query.as_str())
                .into_iter()
                .cloned()
                .collect();

        if filtered.is_empty() {
            return self.render_rail_empty_message(if rows.is_empty() {
                "no worktrees found"
            } else {
                "no worktrees match this filter"
            });
        }

        let repo_inputs: Vec<RepoWorktrees> = self
            .repos
            .iter()
            .map(|repo| RepoWorktrees {
                repo_id: repo.id,
                repo_name: repo.name.clone(),
                rows: if Some(repo.id) == self.focused_repo {
                    filtered.clone()
                } else {
                    Vec::new()
                },
            })
            .collect();
        let groups = rail::group_worktrees_by_repo(repo_inputs);

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

    /// One repo group (§2.0-2.1): the header (name, `N wt` count, and the amber `N worktrees
    /// waiting` when non-zero), then every worktree row already ranked most-urgent-first by
    /// [`rail::group_worktrees_by_repo`].
    pub(in crate::rail) fn render_repo_group(
        &self,
        group: &RepoGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let waiting = group.waiting_count();

        div()
            .id(("repo-group", group.repo_id.0))
            .flex()
            .flex_col()
            .child(
                // Padding `8 12 4` (§2.1).
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .pt(px(8.0))
                    .px(px(12.0))
                    .pb(px(4.0))
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
                            .child(format!("{} wt", group.rows.len())),
                    )
                    .child(div().flex_1())
                    .when(waiting > 0, |el| {
                        el.child(
                            div()
                                .font(font(theme::font::SANS))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_size(self.ui_text_size(9.5))
                                .text_color(theme::status::ASK_CARD_FG)
                                .child(format!("{waiting} worktrees waiting")),
                        )
                    }),
            )
            .children(
                group
                    .rows
                    .iter()
                    .enumerate()
                    .map(|(index, row)| self.render_worktree_row(row, index, cx)),
            )
    }

    /// Whether `row`'s agent rows are currently shown - an explicit per-worktree override in
    /// [`Self::rail_collapse_overrides`] if the caret has ever been clicked for this path,
    /// otherwise the real default (§2.2: "Worktrees whose most urgent agent is idle start
    /// collapsed"). A session-less row has no caret at all, so this is only ever consulted
    /// (via [`Self::render_worktree_row`]) when `row.sessions` is non-empty.
    pub(in crate::rail) fn worktree_is_expanded(&self, row: &WorktreeRow) -> bool {
        match self.rail_collapse_overrides.get(&row.path) {
            Some(expanded) => *expanded,
            None => row.aggregate_status() != Status::Idle,
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

    /// The worktree row's `⚠ N` shared-file flag (§2.2's trailing slot, §4's "files two agents
    /// both wrote") - real, but only for whichever worktree's diff is currently loaded in
    /// Zone 3 ([`Self::diff_root`]): [`Self::file_authorship`] is scoped to that one diff (see
    /// its own docs), not tracked per-worktree yet, so every other row has no data to report and
    /// gets a real `0` (hidden by the render side's own "only when ≥1" gate) rather than a
    /// fabricated count.
    pub(in crate::rail) fn worktree_shared_file_count(&self, worktree_path: &Path) -> usize {
        if worktree_path != self.diff_root {
            return 0;
        }
        let Some(diff) = self.current_diff() else {
            return 0;
        };
        self.file_authorship
            .shared_file_count(diff.files.iter().map(|f| f.path.as_path()))
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

        let is_selected = self.active_session_cwd() == row.path;
        let has_agents = !row.sessions.is_empty();
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

        let shared_count = self.worktree_shared_file_count(&row.path);

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
                    row.sessions.iter().map(|session| session.status).collect();
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
            .when(shared_count > 0, |el| {
                el.child(
                    div()
                        .flex_none()
                        .font(font(theme::font::MONO))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_size(self.ui_text_size(9.0))
                        .text_color(theme::status::ASK_CARD_FG)
                        .child(format!("\u{26a0} {shared_count}")),
                )
            })
            .child(trailing);

        let mut container = div()
            .id(("worktree-group", index as u64))
            .flex()
            .flex_col()
            .child(header);
        if is_expanded {
            for session in &row.sessions {
                container = container.child(self.render_agent_row(session, cx));
            }
        }
        container.into_any_element()
    }

    /// One agent row (§2.3): indented 13, a 1px spine (2px and status-coloured when this is the
    /// globally active session - `Self::sessions::active_id`), exactly two lines - chip/title/
    /// elapsed, then status dot/state word/trailing text/model. Clicking it selects this
    /// session's tab *and* its worktree (`Self::select_session` - already does both: it's the
    /// same real entry point the palette/tab-strip use to jump straight to one session).
    pub(in crate::rail) fn render_agent_row(
        &self,
        session: &SessionRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.sessions.active_id() == Some(session.id);
        let status = session.status;
        let (chip_fg, chip_bg) = work_surface::agent_tint(session.kind);
        let chip_glyph = work_surface::agent_initial(session.kind);
        let state_color: gpui::Rgba = match status {
            Status::Ask | Status::Fail => status.color(),
            _ => theme::text::FAINT.into(),
        };
        let trailing_text = agent_trailing_text(session);
        let trailing_color: gpui::Rgba = if status == Status::Fail {
            theme::button::DANGER_FG.into()
        } else {
            theme::text::FAINT.into()
        };
        let id = session.id;

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
                this.select_session(id, window, cx);
            }))
            .child(
                // Line 1: chip · task title · elapsed.
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(15.0))
                            .h(px(15.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::radius::CHIP)
                            .bg(chip_bg)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(9.0))
                            .text_color(chip_fg)
                            .child(chip_glyph),
                    )
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
                            .child(session.title.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::GHOST)
                            .child(rail::format_elapsed(session.elapsed)),
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
                            .child(session.kind.label()),
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
    /// reads these two fields plus `self.sessions`, so this exercises the same code
    /// `Self::request_prune`/`Self::execute_prune` run in production.
    fn seed_one_prunable_worktree(app: &mut AdeApp, path: PathBuf, branch: &str) {
        app.worktrees.push(WorktreeItem {
            path: path.clone(),
            label: branch.to_string(),
            branch: Some(branch.to_string()),
            is_main: false,
            is_locked: false,
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
/// the worktree and raise this session's tab" click behaviour.
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
            is_locked: false,
            error: None,
        }
    }

    /// §2.2: "Worktrees whose most urgent agent is idle start collapsed" - proven here against
    /// a real running session (never collapsed by default) and a real idle one (collapsed by
    /// default), through `Self::worktree_is_expanded`, the single real place that default lives.
    #[gpui::test]
    fn worktree_is_expanded_defaults_to_the_real_idle_rooted_rule(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt = tempfile::tempdir().expect("tempdir wt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt.path().to_path_buf(), "wt")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.sessions.spawn(
                SessionKind::Shell,
                wt.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });
        // The pty spawn itself is async (`TerminalPane::spawn_process`), so the process isn't
        // observably running yet the instant `spawn` returns - let it actually start before
        // reading a live `Status` off it.
        cx.run_until_parked();

        let running_row = app.read_with(cx, |app, cx| {
            app.build_worktree_rows(cx)
                .into_iter()
                .find(|row| row.path == wt.path())
                .expect("the seeded worktree must produce a row")
        });
        assert_eq!(
            running_row.aggregate_status(),
            Status::Run,
            "sanity check: a just-spawned shell is Run, not Idle, within the recent-output window"
        );
        assert!(
            app.read_with(cx, |app, _| app.worktree_is_expanded(&running_row)),
            "a worktree whose most urgent agent is running must default to expanded"
        );

        // Force the same row into Idle without waiting on a real clock: a session-less
        // `WorktreeRow` (same path, no sessions) aggregates to `Status::Idle` exactly the way a
        // real shell does once it goes quiet past `status::RUN_RECENT_OUTPUT_WINDOW` - the same
        // `aggregate_status` code path `Self::worktree_is_expanded` itself reads.
        let idle_row = rail::WorktreeRow {
            sessions: Vec::new(),
            ..running_row
        };
        assert_eq!(idle_row.aggregate_status(), Status::Idle, "sanity check");
        assert!(
            !app.read_with(cx, |app, _| app.worktree_is_expanded(&idle_row)),
            "an idle-rooted worktree must default to collapsed"
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
            app.sessions.spawn(
                SessionKind::Shell,
                wt.path().to_path_buf(),
                12.0,
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
    /// `Self::render_agent_row`'s own click handler calls exactly `Self::select_session`, so this
    /// exercises that same real call: starting from worktree A selected/focused, selecting a
    /// session that lives in worktree B must move the rail's selection to B *and* make that
    /// exact session the active tab - not just one half of the pair.
    #[gpui::test]
    fn selecting_an_agent_session_selects_its_worktree_and_raises_its_tab(cx: &mut TestAppContext) {
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
        let session_in_b = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.sessions.spawn(
                SessionKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.select_worktree(0, window, cx);
            app.sessions.spawn(
                SessionKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
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
            app.select_session(session_in_b, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.selected),
            Some(1),
            "selecting a session in worktree B must select worktree B in the rail"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(session_in_b),
            "and that exact session must become the active tab"
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
            app.sessions.spawn(
                SessionKind::Claude,
                busy_wt.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.sessions.spawn(
                SessionKind::Codex,
                busy_wt.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        app.update(cx, |app, cx| {
            let _ = app.render_rail_list(cx);
        });
    }
}
