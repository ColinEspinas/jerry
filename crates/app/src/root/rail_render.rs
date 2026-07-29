use super::*;
use crate::root::widgets::{render_keycap_row, KeycapSize};

impl AdeApp {
    pub(super) fn toggle_rail_mode(&mut self, cx: &mut Context<Self>) {
        self.rail_mode = self.rail_mode.toggled();
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Types (or backspaces/clears) into [`Self::filter_query`] - a small, hand-rolled text
    /// field (append/backspace only, no cursor positioning or selection) rather than
    /// `vendor/zed/crates/gpui/examples/input.rs`'s full `EntityInputHandler`, judged out of
    /// scope for a single filter row. Modified keystrokes (⌘, ⌃, ⌥) are left unhandled and
    /// keep propagating, so app-level shortcuts (e.g. ⌘N) still reach their bindings while
    /// this field has focus.
    pub(super) fn handle_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        let changed = match keystroke.key.as_str() {
            "backspace" => self.filter_query.pop().is_some(),
            "escape" => {
                let had_text = !self.filter_query.is_empty();
                self.filter_query.clear();
                had_text
            }
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => {
                    self.filter_query.push_str(text);
                    true
                }
                _ => false,
            },
        };
        if changed {
            self.prune_confirm_armed = false;
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// Builds the rail's per-session rows from live state: each session's `TerminalPane`
    /// (process signal, question preview), the matching worktree's branch name, and the diff
    /// summary from [`Self::diff_cache`] (refreshed by the periodic task started in
    /// `Self::new`). A session with no diff data yet simply shows `0`/`0` until the next
    /// status-poll tick fills it in.
    pub(super) fn build_session_rows(&self, cx: &App) -> Vec<SessionRow> {
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
                }
            })
            .collect()
    }

    /// Builds the "by project" worktree list: every worktree `wt_core::list_worktrees`
    /// reported, including ones that failed to read - `crate::worktrees::WorktreeItem`'s docs
    /// say a per-entry error is kept in the list rather than filtered out, and
    /// `Self::render_worktree_note_row` renders an errored entry as a visible,
    /// non-interactive row.
    ///
    /// Readable entries get their clean/merged note from [`Self::worktree_notes`] (refreshed
    /// by the same periodic task as [`Self::diff_cache`]), defaulting to "unknown yet"
    /// (`clean: None, merge: None`) for one the background snapshot hasn't reached yet.
    pub(super) fn build_worktree_entries(&self) -> Vec<WorktreeEntry> {
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
    /// The status bar's real CPU%/memory sampling (`crate::process_stats`) deliberately rides
    /// this same existing timer rather than spawning a second, independent polling loop -
    /// `prev_process_samples` is the one piece of state that must survive across ticks (a CPU%
    /// needs a delta between two samples), threaded through the loop body itself rather than
    /// stored on `Self`, since nothing outside this loop ever needs the raw, pre-percentage
    /// reading.
    pub(super) fn start_status_polling(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn prunable_worktree_paths(&self) -> Vec<PathBuf> {
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
    pub(super) fn request_prune(&mut self, cx: &mut Context<Self>) {
        let candidates = self.prunable_worktree_paths();

        if candidates.is_empty() {
            self.prune_confirm_armed = false;
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
    pub(super) fn execute_prune(&mut self, candidates: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.prune_in_flight {
            // Defense in depth alongside `Self::render_rail_footer`'s own gating of the prune
            // button while a batch is running.
            self.prune_status = Some("prune already running\u{2026}".to_string());
            cx.notify();
            return;
        }
        let repo_path = self.repo_path.clone();
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
    pub(super) fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("session-rail")
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
                    .id("session-rail-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_rail_list(cx)),
            )
            .child(self.render_rail_footer(cx))
    }

    /// A visible error banner for [`Self::worktrees_error`] (`wt_core::list_worktrees`
    /// failing outright, e.g. a corrupt repository) - shown as a standing banner rather than
    /// replacing the whole session list, so already-open sessions stay usable even when the
    /// worktree listing itself is broken.
    pub(super) fn render_worktrees_error_banner(&self) -> Option<impl IntoElement> {
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
    pub(super) fn render_rail_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(self.render_rail_mode_toggle(cx))
                    .child(self.render_new_session_button(cx)),
            )
    }

    /// The `by urgency ▾ / by project ▾` control.
    pub(super) fn render_rail_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rail-mode-toggle")
            .cursor_pointer()
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .font(font(theme::font::MONO))
            .text_size(self.ui_text_size(10.0))
            .text_color(theme::text::DIM)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child(format!("{} \u{25be}", self.rail_mode.label()))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.toggle_rail_mode(cx);
            }))
    }

    /// The `+` control with its real, platform-resolved `mod+N` keycap pair (`⌘N` on macOS,
    /// `Ctrl N` on Windows/Linux - `crate::keymap::resolve_combo`) - spawns a real new shell
    /// session (see [`NewSession`]'s docs for the judgment call on the keybinding side of
    /// this).
    pub(super) fn render_new_session_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
    pub(super) fn render_rail_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = !self.filter_query.is_empty();

        div()
            .id("rail-filter-row")
            .track_focus(&self.filter_focus_handle)
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
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.5))
                    .text_color(if has_query {
                        theme::text::DIM
                    } else {
                        theme::text::GHOST
                    })
                    .child(if has_query {
                        self.filter_query.clone()
                    } else {
                        "filter sessions".to_string()
                    }),
            )
    }

    /// Dispatches to the real urgency- or project-grouped list, per [`Self::rail_mode`].
    /// Builds [`SessionRow`]s fresh from live state every render (cheap: no I/O, just field
    /// reads plus the cached [`Self::diff_cache`]/[`Self::worktree_notes`] snapshots) - see
    /// [`Self::build_session_rows`]'s docs.
    pub(super) fn render_rail_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let rows = self.build_session_rows(cx);
        match self.rail_mode {
            RailMode::Urgency => self.render_urgency_list(&rows, cx),
            RailMode::Project => self.render_project_list(&rows, cx),
        }
    }

    pub(super) fn render_urgency_list(
        &self,
        rows: &[SessionRow],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let filtered: Vec<SessionRow> = rail::filter_sessions(rows, &self.filter_query)
            .into_iter()
            .cloned()
            .collect();
        let groups = rail::group_by_urgency(&filtered);

        if groups.is_empty() {
            return self.render_rail_empty_message(if rows.is_empty() {
                "no sessions open"
            } else {
                "no sessions match this filter"
            });
        }

        let mut list = div().id("rail-urgency-groups").flex().flex_col();
        for group in &groups {
            list = list.child(self.render_status_group(group, cx));
        }
        list.into_any_element()
    }

    pub(super) fn render_rail_empty_message(&self, message: &'static str) -> gpui::AnyElement {
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

    /// One urgency group: the 5×5 status-colour square + uppercase label + count header
    /// row, then every session row in that status.
    pub(super) fn render_status_group(
        &self,
        group: &StatusGroup,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(("status-group", group.status.urgency_rank() as u64))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .py(px(5.0))
                    .child(div().w(px(5.0)).h(px(5.0)).bg(group.status.color()))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::FAINT)
                            .child(group.status.label().to_uppercase()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::GHOST)
                            .child(group.rows.len().to_string()),
                    ),
            )
            .children(
                group
                    .rows
                    .iter()
                    .map(|row| self.render_session_row(row, 0, cx)),
            )
    }

    /// "By project" mode: a single project header (this app manages exactly one repository -
    /// see the module docs on why multi-project support is out of scope) followed by every
    /// worktree as a child row, indented, each either a real session row or a real
    /// session-less worktree row - see [`rail::build_project_children`]'s docs for why every
    /// worktree appears here, not just ones with an open session.
    pub(super) fn render_project_list(
        &self,
        rows: &[SessionRow],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let worktrees = self.build_worktree_entries();
        let children = rail::build_project_children(&worktrees, rows);
        let filtered = rail::filter_project_children(&children, &self.filter_query);

        if filtered.is_empty() {
            return self.render_rail_empty_message(if children.is_empty() {
                "no worktrees found"
            } else {
                "no worktrees match this filter"
            });
        }

        let project_name = self
            .repo_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repo_path.display().to_string());
        let project_branch = self
            .worktrees
            .iter()
            .find(|item| item.is_main)
            .and_then(|item| item.branch.clone());
        let dots = rail::status_dot_cluster(&children);
        let worktree_count = worktrees.len();

        let mut list = div().id("rail-project").flex().flex_col();
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .h(px(27.0))
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme::text::STRONG)
                        .child(project_name),
                )
                .when_some(project_branch, |el, branch| {
                    el.child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.0))
                            .text_color(theme::text::GHOST)
                            .child(branch),
                    )
                })
                .child(div().flex_1())
                .child(
                    // Aggregate status summary (`rail::status_dot_cluster`): 5×5 dots, same
                    // size as `Self::render_status_group`'s header marker - deliberately
                    // larger than an individual session row's 4×4 dot.
                    div().flex().items_center().gap(px(3.0)).children(
                        dots.into_iter()
                            .map(|status| div().w(px(5.0)).h(px(5.0)).bg(status.color())),
                    ),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("{worktree_count} wt")),
                ),
        );

        for (index, child) in filtered.into_iter().enumerate() {
            list = list.child(self.render_project_child(child, index, cx));
        }

        list.into_any_element()
    }

    /// One indented child row under the project header, with a 1px vertical spine (README:
    /// "indented 16 with a 1px `#1e2225` vertical spine"). `index` is only used to keep
    /// element ids unique for the degenerate case of two error'd `WorktreeEntry`s sharing the
    /// same (empty) path - see `Self::render_worktree_note_row`'s docs.
    pub(super) fn render_project_child(
        &self,
        child: &ProjectChild,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row: gpui::AnyElement = match child {
            ProjectChild::Session(session_row) => self
                .render_session_row(session_row, 0, cx)
                .into_any_element(),
            ProjectChild::Worktree(entry) => self
                .render_worktree_note_row(entry, index, cx)
                .into_any_element(),
        };

        div()
            .flex()
            .pl(px(16.0))
            .border_l_1()
            .border_color(theme::border::ZONE)
            .child(row)
    }

    /// A session-less worktree row in "by project" mode - real path/branch, real
    /// `checkout · clean` / `merged HH:MM · prunable` note (see [`rail::WorktreeNote::
    /// label`]).
    pub(super) fn render_worktree_note_row(
        &self,
        entry: &WorktreeEntry,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = format!("worktree-row-{index}-{}", entry.path.display());

        if let Some(error) = &entry.error {
            // A real error row, per `crate::worktrees::WorktreeItem`'s documented intent:
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
                        .child(entry.label.clone()),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(10.0))
                        .text_color(theme::status::FAIL)
                        .child(error.clone()),
                );
        }

        let path = entry.path.clone();
        div()
            .id(id)
            .cursor_pointer()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .px(px(10.0))
            .py(px(6.0))
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_worktree_by_path(&path, cx);
            }))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(self.ui_text_size(12.0))
                    .text_color(theme::text::BODY)
                    .child(entry.label.clone()),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::GHOST)
                    .child(entry.note.label()),
            )
    }

    /// One session row, exactly per the README's spec: agent badge, title, meta, second line
    /// (status dot + branch + stat), and a question-preview card for waiting sessions.
    /// `indent` is currently always `0` (project mode already indents the whole child row via
    /// [`Self::render_project_child`]'s spine) - kept as a parameter so a future nested
    /// grouping doesn't need to change this method's signature.
    pub(super) fn render_session_row(
        &self,
        row: &SessionRow,
        indent: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.sessions.active_id() == Some(row.id);
        let (badge_fg, badge_bg) = work_surface::agent_tint(row.kind);

        let title_color = if is_selected {
            theme::text::SELECTED
        } else if row.status == Status::Idle {
            theme::text::DIMMER
        } else {
            theme::text::BODY
        };

        let (meta_text, meta_color) = match row.status {
            Status::Ask => ("waiting".to_string(), theme::status::ASK_CARD_FG),
            Status::Fail => ("failed".to_string(), theme::text::GHOST),
            Status::Review => ("ready".to_string(), theme::text::GHOST),
            Status::Run => ("running".to_string(), theme::text::GHOST),
            Status::Idle => ("idle".to_string(), theme::text::GHOST),
        };

        let stat_text = if row.status == Status::Fail {
            row.exit_code
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| "failed".to_string())
        } else if row.add > 0 || row.del > 0 {
            format!("+{} \u{2212}{}", row.add, row.del)
        } else {
            String::new()
        };
        let stat_color = if row.status == Status::Fail {
            theme::button::DANGER_FG
        } else {
            theme::text::GHOST
        };

        let id = row.id;
        let mut container = div()
            .id(("session-row", id))
            .cursor_pointer()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .pl(px(12.0 + indent as f32 * 16.0))
            .pr(px(10.0))
            .pt(px(6.0))
            .pb(px(7.0))
            .border_l(px(2.0))
            .border_color(row.status.color())
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.select_session(id, window, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::radius::CHIP)
                            .bg(badge_bg)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(9.0))
                            .text_color(badge_fg)
                            .child(work_surface::agent_initial(row.kind)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::SANS))
                            .text_size(self.ui_text_size(12.0))
                            .text_color(title_color)
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(meta_color)
                            .child(meta_text),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pt(px(2.0))
                    // 4×4, smaller than the group-header/project-summary dots (5×5) - matches
                    // the mockup's `s.dot`/`r.dot` fixtures.
                    .child(div().w(px(4.0)).h(px(4.0)).bg(row.status.color()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.5))
                            .text_color(if is_selected {
                                theme::text::DIM
                            } else {
                                theme::text::FAINTER
                            })
                            .child(
                                row.branch
                                    .clone()
                                    .unwrap_or_else(|| "(detached)".to_string()),
                            ),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(10.0))
                            .text_color(stat_color)
                            .child(stat_text),
                    ),
            );

        if let Some(preview) = &row.question_preview {
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

        container
    }

    /// The real `Y GB` (`+` suffixed if [`Self::disk_usage`] was truncated) disk-usage label, or
    /// `...` while the background scan hasn't reported a real total yet - shared by
    /// [`Self::render_rail_footer`] and the status bar's worktrees cluster
    /// (`root::status_bar::render_status_worktrees_cluster`), so the two can never format the
    /// same real aggregate differently.
    pub(super) fn disk_usage_label(&self) -> String {
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
    pub(super) fn render_rail_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::GHOST)
                    .child(if let Some(status) = &self.prune_status {
                        status.clone()
                    } else {
                        format!("{worktree_count} worktrees \u{b7} {disk_label}")
                    }),
            )
            .child(prune_button)
    }
}

/// Regression coverage for [`AdeApp::prune_in_flight`] - mirrors
/// `root::merge_flow::merge_regression_tests`'s real-git-repo, deterministic-executor idiom,
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

    /// Same linked-worktree idiom `root::merge_flow`'s test module uses. Created with no new
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
