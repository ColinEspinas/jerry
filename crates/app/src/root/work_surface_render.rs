use super::*;
use crate::root::widgets::{render_action_keycap_row, render_keycap_row, KeycapSize};

impl AdeApp {
    pub(super) fn new_session(&mut self, kind: SessionKind, cx: &mut Context<Self>) {
        let cwd = self.active_session_cwd();
        self.sessions.spawn(kind, cwd, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    pub(super) fn handle_new_session_action(
        &mut self,
        _action: &NewSession,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, cx);
    }

    /// Activates session `id`'s tab and, if it maps to a currently-listed worktree, also
    /// selects that worktree (keeping the right-hand file tree/diff panel in sync with the
    /// session the user just clicked) - see the module docs for why this double duty is a
    /// deliberate integration point rather than the rail owning its own separate notion of
    /// "current worktree": the right sidebar is still driven by [`Self::selected`], since
    /// Zone 2/3 (which the design's state model has as `focused_session`-driven) hasn't been
    /// rebuilt yet.
    pub(super) fn select_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.sessions.set_active(id);
        self.prune_confirm_armed = false;
        let cwd = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| session.cwd.clone());
        if let Some(cwd) = cwd {
            if let Some(index) = self.worktrees.iter().position(|item| item.path == cwd) {
                if self.selected != Some(index) {
                    self.select_worktree(index, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    /// Derives the real [`Status`] for one live session - the single source of truth both
    /// [`Self::build_session_rows`] (the rail) and Zone 2's restyle (the context bar's status
    /// pill, and the CLI/terminal pane header/footer) read, so the rail and the work surface
    /// can never disagree about a session's status. Mirrors `Self::build_session_rows`'s own
    /// prior inline signal-gathering exactly - factored out once a second call site (Zone 2)
    /// needed the identical logic, rather than a second, independently-drifting copy of it.
    pub(super) fn session_status(&self, session: &Session, cx: &App) -> Status {
        let pane = session.pane.read(cx);
        let signal = if pane.is_running() {
            status::ProcessSignal::Running {
                idle: pane.idle_duration().unwrap_or_default(),
            }
        } else if let Some(exit) = pane.exit_status() {
            status::ProcessSignal::Exited {
                success: exit.success(),
            }
        } else if pane.spawn_error().is_some() {
            // A process that never started is a real failure the status derivation should
            // surface, even though it has no `ExitStatus` of its own to report.
            status::ProcessSignal::Exited { success: false }
        } else {
            status::ProcessSignal::NoProcess
        };
        let has_diff = self
            .diff_cache
            .get(&session.cwd)
            .map(|summary| summary.has_changes)
            .unwrap_or(false);
        status::derive_status(session.kind, signal, has_diff)
    }

    /// The context bar's real `Archive` action, and the idle-status footer's own `Archive`
    /// action (`design_handoff_jerry_ade/README.md`'s Session context bar spec: "`Merge`
    /// (outline) · `Archive` (ghost)") - closes the tab via [`Self::close_session`] (see that
    /// method's docs for why every real tab-close path goes through it, not
    /// `Sessions::close` directly).
    pub(super) fn archive_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.close_session(id, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Closes session `id`'s real tab (`Sessions::close` - deterministically tears down its
    /// real child process) and, if `id` is the session whose `Merge` click started
    /// [`Self::merge_flow`], cleans that up too (see [`Self::clear_merge_flow_for_closed_session`]).
    ///
    /// Every real place a session tab closes - [`Self::archive_session`], [`Self::
    /// respawn_session`]'s close-then-respawn, and the tab strip's own `×` - goes through this
    /// one function rather than calling `Sessions::close` directly, so none of them can
    /// independently forget the merge_flow cleanup. This was a real, verified bug: with
    /// `Sessions::close` called from three separate places and only one of them (originally
    /// `archive_session`) clearing `merge_flow`, archiving (or retrying/resuming) the session
    /// that was mid-merge left `Self::merge_flow`'s `session_id` pointing at a session that no
    /// longer existed - Surface D could never render again to finish or abort it, and
    /// `Self::render_merge_button`'s `self.merge_flow.is_some()` disabled check stayed `true`
    /// forever, silently disabling the `Merge` button for *every* session in the app.
    pub(super) fn close_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.sessions.close(id, cx);
        if self
            .merge_flow
            .as_ref()
            .is_some_and(|flow| flow.session_id == id)
        {
            self.clear_merge_flow_for_closed_session(cx);
        }
    }

    /// The surface footer's real `Interrupt ⌃C` action - sends a real `Ctrl-C` to the
    /// session's own pty via `TerminalPane::interrupt`, exactly as if the user had focused the
    /// pane and pressed Ctrl-C themselves.
    pub(super) fn interrupt_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let pane = session.pane.clone();
        pane.update(cx, |pane, cx| pane.interrupt(cx));
    }

    /// The surface footer's real `Retry ⌘R` (failed sessions) / `Resume ⌘⏎` (idle sessions)
    /// action. This app has no saved-session resumability to actually resume *from* (see
    /// `crate::work_surface::pty_state_label`'s docs on the same gap: no `detached ·
    /// resumable` state exists here) - the real, honest equivalent implemented here is: close
    /// this tab, then spawn a fresh session of the same kind into the same worktree, exactly
    /// as if the user had clicked "New ... Session" again themselves. A real action (the old
    /// process is genuinely torn down, a new one genuinely started), just not literally
    /// "resume where it left off" - `crate::work_surface::ActionKind::Respawn`'s docs name
    /// this same trade-off.
    pub(super) fn respawn_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let kind = session.kind;
        let cwd = session.cwd.clone();
        self.close_session(id, cx);
        self.sessions.spawn(kind, cwd, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// The surface footer's real `Open terminal` action - finds an already-open real `Shell`
    /// session in the same worktree and selects it, or spawns one if none exists yet. Real,
    /// minimal: this app's session model has no notion of "the terminal view of *this*
    /// session" (each session is its own independent tab/process - see `crate::sessions`'
    /// module docs), so "open terminal" here means "get me a shell in this worktree", the same
    /// real capability the rail's own "+ New Shell" button already provides.
    pub(super) fn open_companion_terminal(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        let existing = self
            .sessions
            .iter()
            .find(|session| session.kind == SessionKind::Shell && session.cwd == cwd)
            .map(|session| session.id);
        match existing {
            Some(id) => self.select_session(id, cx),
            None => {
                self.sessions.spawn(SessionKind::Shell, cwd, cx);
                self.prune_confirm_armed = false;
                cx.notify();
            }
        }
    }

    /// A real utility row above the tab strip: "+ shell" / "+ claude" / "+ codex" buttons that
    /// spawn a real session into `active_session_cwd()`, plus a reminder of which worktree
    /// that currently resolves to. Not part of `design_handoff_jerry_ade/Jerry.dc.html` (the
    /// mockup's own rail "+"/tab-strip "+" only ever spawn one default kind - real per-kind
    /// selection lives in the design's Settings › Agents page, which is out of scope here) -
    /// kept as a real, restyled (theme-token, not raw `rgb()`) necessity: without it, this app
    /// would have no way to start a `claude`/`codex` session at all.
    pub(super) fn render_session_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cwd = self.active_session_cwd();

        let new_session_button = |label: &'static str, kind: SessionKind| {
            div()
                .id(format!("new-session-{}", kind.label()))
                .cursor_pointer()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(theme::radius::CHIP)
                .bg(theme::surface::CHIP_NEUTRAL)
                .font(font(theme::font::MONO))
                .text_size(px(10.5))
                .text_color(theme::text::DIM)
                .hover(|el| {
                    el.bg(theme::surface::ROW_HOVER_ALT)
                        .text_color(theme::text::PRIMARY)
                })
                .child(label)
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.new_session(kind, cx);
                }))
        };

        div()
            .id("session-toolbar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .px(px(12.0))
            .h(px(30.0))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(new_session_button("+ shell", SessionKind::Shell))
            .child(new_session_button("+ claude", SessionKind::Claude))
            .child(new_session_button("+ codex", SessionKind::Codex))
            .child(div().flex_1())
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(format!("new sessions spawn in: {}", cwd.display())),
            )
    }

    /// The tab strip (34) - `design_handoff_jerry_ade/README.md`'s spec: one 14×14 kind chip
    /// per tab (see [`render_tab_chip`]), active/inactive bg/underline/label colours (see
    /// `crate::work_surface::tab_colors`), a real `+` (spawns a new default shell session,
    /// same real action as the rail's own `+`), and the real, platform-resolved `mod`/`1…8`
    /// keycap hint pinned right (`⌘`/`1…8` on macOS, `Ctrl`/`1…8` on Windows/Linux).
    pub(super) fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = div()
            .id("tab-strip")
            .flex()
            .flex_none()
            .items_stretch()
            .h(theme::band::TAB_STRIP)
            .bg(theme::surface::TITLE_BAR)
            .border_b_1()
            .border_color(theme::border::ZONE);

        for session in self.sessions.iter() {
            bar = bar.child(self.render_session_tab(session, cx));
        }

        bar = bar.child(
            div()
                .id("tab-strip-new")
                .flex_none()
                .flex()
                .items_center()
                .px(px(11.0))
                .cursor_pointer()
                .font(font(theme::font::MONO))
                .text_size(px(13.0))
                .text_color(theme::text::GHOST)
                .hover(|el| el.text_color(theme::text::MUTED))
                .child("+")
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.new_session(SessionKind::Shell, cx);
                })),
        );

        bar.child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .child(render_keycap_row(
                        &keymap::resolve_combo(
                            "mod+1\u{2026}8",
                            self.window_controls_style().is_macos(),
                        ),
                        KeycapSize::Standard,
                    )),
            )
    }

    /// One tab: a 14×14 kind chip, the real label (the resolved binary name for an agent CLI
    /// tab, or the literal `terminal` for a shell tab - `design_handoff_jerry_ade/README.md`'s
    /// own tab-strip spec), and a real `×` that closes it (`Sessions::close`, tearing down the
    /// real process). Split into a `flex_1` clickable content row plus a `flex_none` 1px
    /// underline bar (rather than a single div with two differently-coloured borders) because
    /// GPUI's `Style::border_color` is one uniform colour for every edge
    /// (`vendor/zed/crates/gpui/src/style.rs`) - it cannot give the right border (always
    /// `theme::border::INNER`) and the bottom "underline" (active/inactive-dependent) two
    /// different colours on the same div.
    pub(super) fn render_session_tab(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = session.id;
        let is_active = self.sessions.active_id() == Some(id);
        let chip_kind = work_surface::tab_chip_kind(session.kind);
        let label = match chip_kind {
            work_surface::TabChipKind::Cli => session.pane.read(cx).program_label(),
            work_surface::TabChipKind::Term => "terminal".to_string(),
        };
        let is_mono = matches!(chip_kind, work_surface::TabChipKind::Cli);
        let colors = work_surface::tab_colors(is_active);

        div()
            .id(("session-tab", id))
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            .child(
                div()
                    .id(("session-tab-hit", id))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(13.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.select_session(id, cx);
                    }))
                    .child(render_tab_chip(session.kind, is_active))
                    .child(
                        div()
                            .font(font(if is_mono {
                                theme::font::MONO
                            } else {
                                theme::font::SANS
                            }))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(if is_mono { px(11.0) } else { px(11.5) })
                            .text_color(colors.label)
                            .child(label),
                    )
                    .child(
                        div()
                            .id(("close-session-tab", id))
                            .px(px(2.0))
                            .cursor_pointer()
                            .font(font(theme::font::MONO))
                            .text_size(px(11.0))
                            .text_color(theme::text::GHOST)
                            .hover(|el| el.text_color(theme::button::DANGER_FG))
                            .child("\u{d7}")
                            .on_click(cx.listener(
                                move |this, _event: &ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.close_session(id, cx);
                                    cx.notify();
                                },
                            )),
                    ),
            )
            .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline))
    }

    /// The session context bar (32) - `design_handoff_jerry_ade/README.md`'s spec: agent
    /// badge/name, a divider, real branch, the real worktree path (the one flexible,
    /// ellipsising child - every other child here is `flex_none` and non-wrapping, matching
    /// the README's own "layout rule that matters" so the bar never wraps when the centre
    /// narrows), a real status pill, and `Merge`/`Archive`.
    pub(super) fn render_session_context_bar(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.session_status(session, cx);
        let (agent_fg, agent_bg) = work_surface::agent_tint(session.kind);
        let agent_initial = work_surface::agent_initial(session.kind);
        // The design's `focus.agent` is a *model* name (`sonnet-4.5`, `gpt-5-codex`) this
        // app's `SessionKind` has no equivalent of (it only tracks which CLI *binary* is
        // running, not which model that CLI is configured to use) - `session.kind.label()`
        // ("Claude"/"Codex"/"Shell") is the closest real, honest substitute, rather than
        // fabricating a model name this app never actually observed.
        let agent_label = session.kind.label();
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == session.cwd)
            .and_then(|item| item.branch.clone());
        let worktree_path = session.cwd.display().to_string();
        let id = session.id;

        div()
            .id("session-context-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::band::CONTEXT_BAR)
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .w(px(15.0))
                    .h(px(15.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(agent_bg)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(8.5))
                    .text_color(agent_fg)
                    .child(agent_initial),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(theme::text::MUTED)
                    .child(agent_label),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(13.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::DIM)
                    .child(branch.unwrap_or_else(|| "(detached)".to_string())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::PATH)
                    .child(worktree_path),
            )
            .child(render_status_pill(status_value))
            .child(self.render_merge_button(id, cx))
            .child(self.render_archive_button(id, cx))
    }

    /// The context bar's real `Merge` button - see [`Self::start_merge`]'s docs for the real
    /// `wt_core::merge::attempt_merge` call it starts. Disabled (dimmed, non-interactive - the
    /// design's own "Accept file" precedent: "dimmed ... never a button that looks clickable
    /// but silently does nothing") whenever *any* merge flow is already active, own session or
    /// not (`Self::start_merge`'s docs on why only one runs at a time), and shows `Merging…`
    /// in place of `Merge` while this specific session's own attempt is the one running.
    pub(super) fn render_merge_button(
        &self,
        id: SessionId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active_for_this_session = self
            .merge_flow
            .as_ref()
            .is_some_and(|flow| flow.session_id == id);
        let running = active_for_this_session
            && matches!(
                self.merge_flow.as_ref().map(|flow| &flow.state),
                Some(merge::MergeFlowState::Running)
            );
        let disabled = self.merge_flow.is_some();
        let label = if running { "Merging\u{2026}" } else { "Merge" };

        let base = div()
            .id(("context-bar-merge", id))
            .flex_none()
            .h(px(20.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .border_1()
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.5))
            .child(label);

        if disabled {
            base.cursor_default()
                .border_color(theme::border::BUTTON_DISABLED)
                .text_color(theme::text::GHOSTER)
        } else {
            base.cursor_pointer()
                .border_color(theme::border::BUTTON)
                .text_color(theme::text::SECONDARY)
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.start_merge(id, cx);
                }))
        }
    }

    /// The context bar's real `Archive` button - see [`Self::archive_session`]'s docs.
    pub(super) fn render_archive_button(
        &self,
        id: SessionId,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(("context-bar-archive", id))
            .flex_none()
            .cursor_pointer()
            .h(px(20.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.5))
            .text_color(theme::text::FAINT)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child("Archive")
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.archive_session(id, cx);
            }))
    }

    /// Surface A/B's shared 27px header - `design_handoff_jerry_ade/README.md`'s Surface A
    /// spec (`claude --resume 3d91e07`-style command, `pid 48213`, right-aligned pty state)
    /// and Surface B spec (`zsh` + worktree path). This app has no saved-session resumability
    /// (no `--resume <sha>` to show - see `crate::work_surface::pty_state_label`'s docs), so
    /// the left label is the real resolved program name alone, never a fabricated resume
    /// argument.
    pub(super) fn render_pty_header(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = session.pane.read(cx);
        let program_label = pane.program_label();
        let pid = pane.pid();
        let is_running = pane.is_running();
        let exit_code = pane.exit_status().map(|status| status.exit_code());
        let status_value = self.session_status(session, cx);
        let state_label = work_surface::pty_state_label(is_running, status_value, exit_code);

        let header = div()
            .id("pty-header")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(9.0))
            .px(px(12.0))
            .h(theme::band::PTY_HEADER)
            .bg(theme::surface::FOOTER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::DIM)
                    .child(program_label),
            );

        let header = match session.kind {
            SessionKind::Shell => header.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child(session.cwd.display().to_string()),
            ),
            SessionKind::Claude | SessionKind::Codex => {
                let header = match pid {
                    Some(pid) => header.child(
                        div()
                            .flex_none()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.5))
                            .text_color(theme::text::GHOST)
                            .child(format!("pid {pid}")),
                    ),
                    None => header,
                };
                header.child(div().flex_1())
            }
        };

        header.child(
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::HINT)
                .child(state_label),
        )
    }

    /// Surface A/B's shared 28px footer - `design_handoff_jerry_ade/README.md`'s Surface A
    /// spec: the `Jerry` word plus git-level actions appropriate to the session's real status.
    /// See `crate::work_surface::footer_actions`/[`Self::render_footer_action_button`] for
    /// which of those actions are real-and-minimal versus honestly disabled this phase.
    pub(super) fn render_pty_footer(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.session_status(session, cx);
        let is_running = session.pane.read(cx).is_running();
        let actions = work_surface::footer_actions(status_value);
        let id = session.id;
        let cwd = session.cwd.clone();

        let mut footer = div()
            .id("pty-footer")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .px(px(12.0))
            .h(theme::band::SURFACE_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.0))
                    .text_color(theme::text::GHOSTER)
                    .child("JERRY"),
            );

        for action in actions {
            let mut enabled = action.implemented;
            // `Resume` (idle status) only means something for a session that has actually
            // exited/never started - a *live*, merely-idle shell has nothing to "resume" (see
            // `crate::work_surface::ActionKind::Respawn`'s docs); real-disable it in that one
            // case rather than letting a click spawn a redundant duplicate session next to a
            // still-running one.
            if action.kind == work_surface::ActionKind::Respawn
                && status_value == Status::Idle
                && is_running
            {
                enabled = false;
            }
            footer = footer.child(self.render_footer_action_button(
                id,
                cwd.clone(),
                action,
                enabled,
                cx,
            ));
        }

        footer.child(div().flex_1())
    }

    /// One footer action button - real (`cursor_pointer`, hover, a real `on_click` dispatch on
    /// `action.kind`) when `enabled`, otherwise the design's own "dimmed, real-disabled"
    /// treatment (no cursor/hover/click at all) - never a button that looks clickable but
    /// silently does nothing.
    pub(super) fn render_footer_action_button(
        &self,
        id: SessionId,
        cwd: PathBuf,
        action: work_surface::FooterAction,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = work_surface::action_button_colors(action.style);
        let label = action.label;
        let kind = action.kind;

        let mut button = div()
            .id(format!("footer-action-{id}-{label}"))
            .h(px(23.0))
            .px(px(10.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(if enabled {
                colors.bg
            } else {
                // A disabled action must never keep its real, full-color fill (that would
                // make an inert button look as, or more, clickable than a real one - exactly
                // what this project's "no fake functionality" rule exists to prevent; a real
                // disabled blue "Resume" was found rendering with the full solid `#243c50`
                // fill next to a real, working "Archive" button). The design itself has no
                // separate disabled-background token - its own disabled precedent
                // (`design_handoff_jerry_ade/README.md`'s "Accept file is always rendered,
                // dimmed (`#454b51` / border `#1f2327`) when there is nothing to accept", and
                // the `Outline`/`Ghost` button styles above, which are already `TRANSPARENT`
                // at full strength) dims only fg/border, never bg - so falling back to
                // `TRANSPARENT` here lets the footer's own background
                // (`theme::surface::FOOTER`) show through and the button visually recede,
                // consistent with that precedent rather than inventing a new muted-fill token.
                work_surface::TRANSPARENT
            })
            .border_1()
            .border_color(if enabled {
                colors.border
            } else {
                theme::border::BUTTON_DISABLED
            })
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        colors.fg
                    } else {
                        theme::text::GHOSTER
                    })
                    .child(label),
            );

        if let Some(spec) = action.keycap {
            let (keycap_fg, keycap_border) = if enabled {
                (colors.keycap_fg, colors.keycap_border)
            } else {
                (theme::text::GHOSTER, theme::border::BUTTON_DISABLED)
            };
            let parts = keymap::resolve_combo(spec, self.window_controls_style().is_macos());
            button = button.child(render_action_keycap_row(&parts, keycap_fg, keycap_border));
        }

        if enabled {
            button = button
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(
                    cx.listener(move |this, _event: &ClickEvent, _window, cx| match kind {
                        work_surface::ActionKind::Interrupt => this.interrupt_session(id, cx),
                        work_surface::ActionKind::OpenTerminal => {
                            this.open_companion_terminal(cwd.clone(), cx)
                        }
                        work_surface::ActionKind::Respawn => this.respawn_session(id, cx),
                        work_surface::ActionKind::Archive => this.archive_session(id, cx),
                        work_surface::ActionKind::Unimplemented => {}
                    }),
                );
        } else {
            button = button.cursor_default();
        }

        button
    }

    /// The centre pane's content: either the real active session's terminal (the pre-existing
    /// behavior), or - while [`Self::open_change`] names a file - that file's real Surface C
    /// (`Self::render_code_surface`): its real diff if one is loaded and `Self::code_view` is
    /// `Diff`, or its real syntax-highlighted File view otherwise (`crate::code_view`).
    ///
    /// A file opened via a Changes-row click (`Self::open_change_diff`) always has a real
    /// `DiffFile` to show; a file opened via a Files-tree row click (`Self::open_file_view`) may
    /// not (browsing an unchanged file), in which case `diff_file` below is simply `None` and
    /// `Self::render_code_surface` shows the File view unconditionally - see that method's docs.
    ///
    /// The terminal-surface fallback path's own root div keeps its historically load-bearing
    /// `.min_w_0()` (an earlier step's real fix for "typing in the terminal pushes the file
    /// tree off-screen" - GPUI's flexbox layout gives a flex item's minimum width its
    /// *content's* intrinsic width by default, so an unbroken wide terminal row could otherwise
    /// grow this pane past its `flex_1` share and push the fixed-width right sidebar off
    /// screen; `.min_w_0()` zeroes that automatic minimum, confirmed against
    /// `vendor/zed/crates/workspace/src/status_bar.rs`'s own real `.flex_1().min_w_0()` use for
    /// exactly this situation).
    pub(super) fn render_center_pane(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(open_path) = self.open_change.clone() {
            // `Self::open_diff_file_cache` already holds the real, up-to-date `DiffFile` for
            // `open_path` (kept fresh by `Self::refresh_open_diff_file_cache`, called at every
            // real point that could change it - never here). Moving it out (rather than cloning
            // it again on every render) is what `Self::render_code_surface` needing `&mut self`
            // requires: a live `&DiffFile` borrowed from `self` can't be held across a call that
            // also needs to mutably borrow all of `self`. `Option::take` is an `O(1)` pointer
            // swap, not a second deep clone of every one of the file's hunks - see
            // `open_diff_file_cache`'s own docs for the per-render clone this replaces.
            let diff_file = self.open_diff_file_cache.take();
            let has_diff_or_file_view =
                diff_file.is_some() || self.code_view == code_view::CodeView::File;
            if has_diff_or_file_view {
                let surface = self.render_code_surface(&open_path, diff_file.as_ref(), cx);
                self.open_diff_file_cache = diff_file;
                return surface;
            }
            self.open_diff_file_cache = diff_file;
        }

        let surface = div()
            .id("work-surface")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme::surface::CENTER)
            .child(self.render_session_toolbar(cx))
            .child(self.render_tab_strip(cx));

        match self.sessions.active() {
            Some(session) => {
                let body = if self
                    .merge_flow
                    .as_ref()
                    .is_some_and(|flow| flow.session_id == session.id)
                {
                    self.render_merge_flow_surface(session, cx)
                } else {
                    div()
                        .id("pty-surface")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_hidden()
                        .bg(theme::surface::PTY)
                        .child(self.render_pty_header(session, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .overflow_hidden()
                                .child(session.pane.clone().into_any_element()),
                        )
                        .child(self.render_pty_footer(session, cx))
                        .into_any_element()
                };
                surface
                    .child(self.render_session_context_bar(session, cx))
                    .child(body)
            }
            None => surface.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::FAINT)
                    .child("no sessions open - start one with the buttons above"),
            ),
        }
        .into_any_element()
    }
}

/// The tab strip's 14×14 kind chip - a `❯` glyph tinted with the session's real agent colour
/// for agent CLI tabs, or the pane glyph (a 14×4 bar plus a 5×2 prompt mark, per
/// `design_handoff_jerry_ade/README.md`'s tab-strip spec) for terminal tabs. Turns
/// `crate::work_surface::tab_chip_kind`/`tab_chip_colors`'s real, unit-tested mapping into
/// actual GPUI elements - no chip-selection *logic* lives here.
pub(super) fn render_tab_chip(kind: SessionKind, active: bool) -> gpui::AnyElement {
    let colors = work_surface::tab_chip_colors(kind, active);
    let base = div()
        .flex_none()
        .w(px(14.0))
        .h(px(14.0))
        .rounded(theme::radius::CHIP)
        .bg(colors.bg);

    match work_surface::tab_chip_kind(kind) {
        work_surface::TabChipKind::Cli => base
            .flex()
            .items_center()
            .justify_center()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(8.0))
            .text_color(colors.fg)
            .child("\u{276f}")
            .into_any_element(),
        work_surface::TabChipKind::Term => base
            .relative()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .w(px(14.0))
                    .h(px(4.0))
                    .bg(colors.fg),
            )
            .child(
                div()
                    .absolute()
                    .left(px(3.0))
                    .top(px(7.0))
                    .w(px(5.0))
                    .h(px(2.0))
                    .rounded(px(1.0))
                    .bg(colors.fg),
            )
            .into_any_element(),
    }
}

/// The session context bar's real status pill - `design_handoff_jerry_ade/README.md`: "status
/// pill (19 high, radius 3, 5px dot + 10px/500 label in the status colour)".
pub(super) fn render_status_pill(status: Status) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(19.0))
        .px(px(7.0))
        .rounded(theme::radius::CHIP)
        .bg(status.pill_bg())
        .child(
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded(px(2.5))
                .bg(status.color()),
        )
        .child(
            div()
                .flex_none()
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(10.0))
                .text_color(status.color())
                .child(status.label()),
        )
}
