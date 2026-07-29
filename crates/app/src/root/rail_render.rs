use super::*;
use crate::root::widgets::{render_keycap_row, KeycapSize};

impl AdeApp {
    pub(super) fn toggle_rail_mode(&mut self, cx: &mut Context<Self>) {
        self.rail_mode = self.rail_mode.toggled();
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Types (or backspaces/clears) into [`Self::filter_query`] - a small, deliberately
    /// minimal hand-rolled text field (append/backspace only, no cursor positioning or
    /// selection), mirroring `crate::terminal_pane::keystroke_to_bytes`'s own "small,
    /// deliberate subset" scope cut rather than porting `vendor/zed/crates/gpui/examples/
    /// input.rs`'s full `EntityInputHandler` (IME marked-text, mouse selection, clipboard) -
    /// judged out of scope for a single filter row. Modified keystrokes (⌘, ⌃, ⌥) are left
    /// unhandled and keep propagating, so app-level shortcuts (e.g. ⌘N) still reach their
    /// bindings while this field has focus.
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

    /// Builds the rail's real per-session rows from live state: each session's
    /// `TerminalPane` (process signal, question preview - see `crate::terminal_pane`'s new
    /// `is_running`/`idle_duration`/`exit_status`/`spawn_error`/`visible_text_lines`
    /// getters), the matching worktree's real branch name, and the real diff summary from
    /// [`Self::diff_cache`] (refreshed by the periodic task started in `Self::new`). No
    /// field here is fabricated or hardcoded - a session with no diff data yet simply shows
    /// `0`/`0` until the next status-poll tick fills it in.
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

    /// Builds the real "by project" worktree list: **every** worktree `wt_core::
    /// list_worktrees` reported, including ones that failed to read - `crate::worktrees::
    /// WorktreeItem`'s own docs say a per-entry error is kept in the list "so the problem is
    /// visible rather than the entry silently vanishing", and filtering them out here would
    /// defeat that intent (a real Phase-A behavior this rewrite must not regress). An
    /// errored item gets a [`WorktreeEntry`] with `error: Some(..)` and an empty note
    /// (nothing real to compute a clean/merged state from); `crate::root::AdeApp::
    /// render_worktree_note_row` renders that as a visible, non-interactive error row rather
    /// than a normal clickable one.
    ///
    /// Readable entries get their real clean/merged note from [`Self::worktree_notes`]
    /// (refreshed by the same periodic task as [`Self::diff_cache`]) - defaulting to an
    /// "unknown yet" note (`clean: None, merge: None`) for one the background snapshot
    /// hasn't reached yet, rather than guessing.
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

    /// Starts the rail's periodic real-status background refresh (see
    /// [`STATUS_POLL_INTERVAL`]'s docs). Every tick: snapshots the current worktree paths and
    /// open sessions' cwds on the foreground thread (cheap, no I/O), computes a real
    /// [`rail::StatusSnapshot`] on the background executor (real `git`/`gix` calls - see
    /// `rail::compute_status_snapshot`'s docs), then writes the result back into
    /// [`Self::diff_cache`]/[`Self::worktree_notes`] on the foreground thread. Mirrors the
    /// same "gather on foreground, compute on background, write back on foreground" shape
    /// [`Self::load_worktrees`]/[`Self::load_diff`] already use.
    pub(super) fn start_status_polling(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(STATUS_POLL_INTERVAL).await;

            let Ok((worktrees, diff_paths)) = this.update(cx, |this, _cx| {
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
                (worktrees, diff_paths)
            }) else {
                break;
            };

            let snapshot = cx
                .background_executor()
                .spawn(async move { rail::compute_status_snapshot(&worktrees, &diff_paths) })
                .await;

            let updated = this.update(cx, |this, cx| {
                this.diff_cache = snapshot.diffs;
                this.worktree_notes = snapshot.worktree_notes;
                cx.notify();
            });
            if updated.is_err() {
                break;
            }
        });
        self._status_poll_task = Some(task);
    }

    /// The rail footer's real `prune` action: removes every currently-known real prune
    /// candidate (not the main checkout, clean, merged - see [`rail::WorktreeNote::
    /// is_prunable`]) via the real, already-tested `wt_core::remove_worktree` (with
    /// `force: false`, so its own dirty-tree refusal still guards against a race between the
    /// last status snapshot and this click), then reloads the real worktree list. Real
    /// functionality, not a decorative label: this can and does delete real directories on
    /// disk.
    /// The real prune candidate list: every worktree that is a prune candidate on its own
    /// merits ([`rail::is_prunable`]) **and** has no live session currently running with its
    /// cwd inside it - see [`rail::prunable_worktree_paths`]'s docs for why that second
    /// condition is not optional. Shared by the footer's displayed count and the actual
    /// removal, so what's shown always matches what a click will really do.
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
    /// two-click confirmation, not a single unconfirmed click: the first click only arms
    /// [`Self::prune_confirm_armed`] and changes the button's own label (real, visible
    /// feedback - see `Self::render_rail_footer`), and does not touch the filesystem at all.
    /// Only a *second* click while already armed calls [`Self::execute_prune`]. This matters
    /// beyond the design's own footer-label spec: `wt_core::is_dirty` correctly follows
    /// git's own ignored-file semantics, so a "clean" worktree can still hold real,
    /// gitignored state (secrets, build artifacts, uncommitted-but-ignored work) that a
    /// single misclick would otherwise destroy silently, for potentially several worktrees
    /// at once.
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

    /// Actually removes `candidates` via the real, already-tested `wt_core::remove_worktree`.
    /// Only ever called once [`Self::request_prune`]'s confirmation step has been satisfied,
    /// and only with paths [`Self::prunable_worktree_paths`] itself produced (never the main
    /// checkout, never a locked worktree, never one with a live session).
    pub(super) fn execute_prune(&mut self, candidates: Vec<PathBuf>, cx: &mut Context<Self>) {
        let repo_path = self.repo_path.clone();
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

    /// A real, visible error banner for [`Self::worktrees_error`] - a real Phase-A behavior
    /// (`wt_core::list_worktrees` failing outright, e.g. a corrupt repository) this rewrite
    /// must not silently drop: the old sidebar returned early with exactly this message; the
    /// rail shows it as a standing banner instead (rather than replacing the whole session
    /// list) so real, already-open sessions stay visible and usable even when the worktree
    /// listing itself is broken.
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
                .text_size(px(10.0))
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
                    .text_size(px(10.0))
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
            .text_size(px(10.0))
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
                    .text_size(px(11.0))
                    .child("+"),
            )
            .child(render_keycap_row(
                &keymap::resolve_combo("mod+N", self.window_controls_style.is_macos()),
                KeycapSize::Standard,
            ))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.new_session(SessionKind::Shell, cx);
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
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child("/"),
            )
            .child(
                div()
                    .flex_1()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
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
                    .text_size(px(10.5))
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
                            .text_size(px(9.5))
                            .text_color(theme::text::FAINT)
                            .child(group.status.label().to_uppercase()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(9.5))
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
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme::text::STRONG)
                        .child(project_name),
                )
                .when_some(project_branch, |el, branch| {
                    el.child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::GHOST)
                            .child(branch),
                    )
                })
                .child(div().flex_1())
                .child(
                    div().flex().items_center().gap(px(3.0)).children(
                        dots.into_iter()
                            .map(|status| div().w(px(5.0)).h(px(5.0)).bg(status.color())),
                    ),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(9.5))
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
                        .text_size(px(12.0))
                        .text_color(theme::status::FAIL)
                        .child(entry.label.clone()),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
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
                    .text_size(px(12.0))
                    .text_color(theme::text::BODY)
                    .child(entry.label.clone()),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
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
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_session(id, cx);
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
                            .text_size(px(9.0))
                            .text_color(badge_fg)
                            .child(work_surface::agent_initial(row.kind)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::SANS))
                            .text_size(px(12.0))
                            .text_color(title_color)
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(9.5))
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
                    .child(div().w(px(4.0)).h(px(4.0)).bg(row.status.color()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.5))
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
                            .text_size(px(10.0))
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
                    .text_size(px(10.5))
                    .text_color(theme::status::ASK_CARD_FG)
                    .child(preview.clone()),
            );
        }

        container
    }

    /// Footer 28: real aggregate stats (`N worktrees · disk usage`) plus the real `prune`
    /// action.
    pub(super) fn render_rail_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Includes error'd entries: `crate::worktrees::WorktreeItem`'s own docs say a real
        // count of what `wt_core::list_worktrees` reported should stay visible problems and
        // all, not silently shrink because some entries failed to read.
        let worktree_count = self.worktrees.len();
        let disk_label = match self.disk_usage {
            Some((bytes, truncated)) => {
                let label = rail::format_bytes(bytes);
                if truncated {
                    format!("{label}+")
                } else {
                    label
                }
            }
            None => "...".to_string(),
        };
        let prunable_count = self.prunable_worktree_paths().len();
        let prune_label = if self.prune_confirm_armed {
            format!("confirm prune ({prunable_count})?")
        } else {
            format!("prune ({prunable_count})")
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
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(if let Some(status) = &self.prune_status {
                        status.clone()
                    } else {
                        format!("{worktree_count} worktrees \u{b7} {disk_label}")
                    }),
            )
            .child(
                div()
                    .id("rail-prune")
                    .cursor_pointer()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(if prunable_count > 0 {
                        theme::button::DANGER_FG
                    } else {
                        theme::text::DISABLED
                    })
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child(prune_label)
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.request_prune(cx);
                    })),
            )
    }
}
