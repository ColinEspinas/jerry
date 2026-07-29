use super::*;
use crate::root::widgets::{render_action_keycap_row, render_keycap_row, KeycapSize};

/// Defines one `JumpToSessionN` real action handler - see [`AdeApp::jump_to_session_at`]'s docs
/// for why eight separate, near-identical handlers (one per real keystroke `secondary-1`..
/// `secondary-8`, registered in `crate::default_key_bindings`) are the correct shape here, not
/// avoidable duplication: a `gpui::KeyBinding` maps one keystroke to one action *value*, and
/// `actions!`-generated unit structs carry no positional data a single shared handler could
/// branch on - so eight distinct action *types* each need their own `on_action` handler
/// regardless. This macro exists only so the eight bodies (each just forwarding a literal
/// position to [`AdeApp::jump_to_session_at`]) can't drift from one another the way eight
/// separately hand-copied functions could.
macro_rules! session_jump_action_handler {
    ($fn_name:ident, $action:ty, $position:expr) => {
        pub(super) fn $fn_name(
            &mut self,
            _action: &$action,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.jump_to_session_at($position, window, cx);
        }
    };
}

impl AdeApp {
    pub(super) fn new_session(
        &mut self,
        kind: SessionKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.active_session_cwd();
        self.sessions.spawn(kind, cwd, cx);
        self.focus_newly_spawned_session(window, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Moves real keyboard focus onto whichever session [`Sessions::spawn`] just made active -
    /// but only when doing so is actually correct: a file tab ([`Self::open_change`]), not a
    /// session's own `TerminalPane`, is what [`Self::render_center_pane`] renders whenever one
    /// is active, so focusing a session's pane while that's true would point `Window::focus` at
    /// a node nothing in the rendered tree tracks - the exact real bug [`Sessions::spawn`]'s own
    /// docs describe. Shared by every real call site that spawns a session, so none of them can
    /// independently get this guard wrong.
    pub(super) fn focus_newly_spawned_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_change.is_none() {
            self.sessions.focus_active(window, cx);
        }
    }

    pub(super) fn handle_new_session_action(
        &mut self,
        _action: &NewSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, window, cx);
    }

    /// Activates session `id`'s tab and, if it maps to a currently-listed worktree, also
    /// selects that worktree (keeping the right-hand file tree/diff panel in sync with the
    /// session the user just clicked) - see the module docs for why this double duty is a
    /// deliberate integration point rather than the rail owning its own separate notion of
    /// "current worktree": the right sidebar is still driven by [`Self::selected`], since
    /// Zone 2/3 (which the design's state model has as `focused_session`-driven) hasn't been
    /// rebuilt yet.
    ///
    /// Since the tab strip unified sessions and file tabs (`design_handoff_jerry_ade/revision/
    /// CHANGELOG.md`'s 2026-07-29 entry, change 4), clicking a session tab while a file tab is
    /// active must switch the centre pane back to showing this session - `Self::open_change`
    /// only ever names a file *or* lets a session's own pane show through (see that field's
    /// docs), never both. This deactivates the file tab (`Self::open_change = None`) without
    /// closing it - it stays in [`Self::open_files`], exactly like clicking a different browser
    /// tab doesn't close the one you switched away from - and restores real keyboard focus onto
    /// the newly active session's pane the same documented way [`Self::close_file_tab`] does when
    /// no file tabs are left, reusing the same [`restore_focus`] helper rather than a fourth
    /// hand-copied focus-restore block (see that function's own docs for why).
    pub(super) fn select_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sessions.set_active(id);
        self.prune_confirm_armed = false;
        if self.open_change.is_some() {
            self.open_change = None;
            self.refresh_open_diff_file_cache();
            self.hover = None;
            restore_focus(
                &self.sessions,
                &mut self.code_return_focus,
                &mut self.code_opened_session,
                window,
                cx,
            );
        }
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
    pub(super) fn archive_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_session(id, window, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Closes session `id`'s real tab (`Sessions::close` - deterministically tears down its
    /// real child process, and moves real keyboard focus onto whichever session becomes active
    /// as a result - see that method's own docs for the real "no file tab is showing instead"
    /// guard it applies) and, if `id` is the session whose `Merge` click started
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
    pub(super) fn close_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let file_tab_active = self.open_change.is_some();
        self.sessions.close(id, file_tab_active, window, cx);
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
    pub(super) fn respawn_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let kind = session.kind;
        let cwd = session.cwd.clone();
        self.close_session(id, window, cx);
        self.sessions.spawn(kind, cwd, cx);
        self.focus_newly_spawned_session(window, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// The surface footer's real `Open terminal` action - finds an already-open real `Shell`
    /// session in the same worktree and selects it, or spawns one if none exists yet. Real,
    /// minimal: this app's session model has no notion of "the terminal view of *this*
    /// session" (each session is its own independent tab/process - see `crate::sessions`'
    /// module docs), so "open terminal" here means "get me a shell in this worktree", the same
    /// real capability the rail's own "+ New Shell" button already provides.
    pub(super) fn open_companion_terminal(
        &mut self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .sessions
            .iter()
            .find(|session| session.kind == SessionKind::Shell && session.cwd == cwd)
            .map(|session| session.id);
        match existing {
            Some(id) => self.select_session(id, window, cx),
            None => {
                self.sessions.spawn(SessionKind::Shell, cwd, cx);
                self.focus_newly_spawned_session(window, cx);
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
                .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                    this.new_session(kind, window, cx);
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

    /// The tab strip (34) - `design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29
    /// entry, change 4: "Was: three fixed peer tabs acting as a pane selector (agent | terminal
    /// | file). Now: agent tab + shell tab + one tab per open file, in open order." Renders one
    /// [`render_session_tab`] per real, live [`crate::sessions::Session`] (unchanged rendering/
    /// behavior from before this change - agent CLI and shell sessions are still peer tabs, one
    /// per session), followed by one [`Self::render_file_tab`] per entry of [`Self::open_files`]
    /// in that `Vec`'s own order - the real, unified tab list this change introduces. Then the
    /// real `+` menu button ([`Self::render_tab_strip_plus`]), and the real, right-aligned
    /// session-jump keycaps + `session` label (replacing the old decorative `mod`/`1…8` pair -
    /// see [`Self::jump_to_session_at`]'s docs for why that pair was never a real binding until
    /// now).
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

        for path in &self.open_files {
            bar = bar.child(self.render_file_tab(path, cx));
        }

        bar = bar.child(self.render_tab_strip_plus(cx));

        // Only real keycaps for sessions that actually exist - "don't show keycap 6 if there
        // are only 4 sessions" (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29
        // entry, change 4). `secondary-1`..`secondary-8` are the only ones actually bound (see
        // `crate::default_key_bindings`), so this never shows a keycap past 8 either.
        let session_count = self.sessions.iter().count().min(8);
        let jump_keys: Vec<String> = (1..=session_count).map(|n| n.to_string()).collect();

        bar.child(div().flex_1()).child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .child(render_keycap_row(&jump_keys, KeycapSize::Standard))
                .child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(px(10.0))
                        .text_color(theme::text::PATH)
                        .child("session"),
                ),
        )
    }

    /// A file tab - `design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29 entry, change
    /// 4: the file's real language chip (same table as the file tree, `file_tree::
    /// lang_chip_for_name`, dimmed to the same neutral a session tab's own chip dims to when
    /// inactive - see `crate::work_surface::file_tab_chip_colors`'s docs), the file's real name,
    /// and a real 15×15 close hit box (hover `theme::surface::TAB_CLOSE_HOVER`, `×` coloured
    /// `theme::text::DIMMER` on the active tab or `theme::text::DISABLED` otherwise). Clicking
    /// the tab body activates it ([`Self::activate_file_tab`]); clicking the `×` closes it for
    /// real ([`Self::close_file_tab`]) and stops the click from also bubbling up to the body's
    /// own activate handler (`cx.stop_propagation()`, the same pattern
    /// [`render_session_tab`]'s own close button already uses). Same active/inactive bg/
    /// underline/label colours as a session tab (`crate::work_surface::tab_colors`) - a file
    /// tab is a real peer of a session tab now, not a second, differently-styled kind of thing.
    pub(super) fn render_file_tab(&self, path: &Path, cx: &mut Context<Self>) -> impl IntoElement {
        let is_active = self.open_change.as_deref() == Some(path);
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let lang = file_tree::lang_chip_for_name(&file_name);
        let chip_colors = work_surface::file_tab_chip_colors(lang, is_active);
        let colors = work_surface::tab_colors(is_active);
        let close_color = if is_active {
            theme::text::DIMMER
        } else {
            theme::text::DISABLED
        };
        let activate_path = path.to_path_buf();
        let close_path = activate_path.clone();
        let key = path.display().to_string();

        div()
            .id(format!("file-tab-{key}"))
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            .child(
                div()
                    .id(format!("file-tab-hit-{key}"))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(13.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        this.activate_file_tab(activate_path.clone(), window, cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .w(px(14.0))
                            .h(px(14.0))
                            .rounded(theme::radius::CHIP)
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(chip_colors.bg)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(7.0))
                            .text_color(chip_colors.fg)
                            .child(lang.label),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(11.0))
                            .text_color(colors.label)
                            .child(file_name),
                    )
                    .child(
                        div()
                            .id(format!("close-file-tab-{key}"))
                            .w(px(15.0))
                            .h(px(15.0))
                            .rounded(theme::radius::CHIP)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|el| el.bg(theme::surface::TAB_CLOSE_HOVER))
                            .font(font(theme::font::MONO))
                            .text_size(px(11.0))
                            .text_color(close_color)
                            .child("\u{d7}")
                            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.close_file_tab(close_path.clone(), window, cx);
                            })),
                    ),
            )
            .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline))
    }

    /// The tab strip's real `+` menu button (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s
    /// 2026-07-29 entry, change 4: "`+` became a menu button (`+ ▾`, active bg `#171a1d`)
    /// opening a 326-wide popover") - toggles [`Self::plus_menu_open`] rather than
    /// unconditionally spawning a shell the way this button used to (that real, always-shell
    /// behavior stays on the rail's own separate `+`, which the design doesn't respec - see
    /// [`crate::root::rail_render::render_new_session_button`]). A `gpui::canvas` child captures
    /// this button's own real painted bounds into [`Self::plus_button_bounds`] on every render -
    /// see that field's docs for why [`Self::render_plus_menu`] positions the popover off of it
    /// rather than a second, hand-computed offset. Opening the menu also kicks off a real
    /// [`Self::load_agent_rows`] refresh, so the "New agent pane" row's own sub-label
    /// ([`Self::resolved_new_agent_kind`]) reflects a reasonably fresh `$PATH` search rather than
    /// whatever (possibly empty, if Settings was never opened) snapshot happened to be cached.
    pub(super) fn render_tab_strip_plus(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let is_open = self.plus_menu_open;

        div()
            .id("tab-strip-new")
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.0))
            .px(px(10.0))
            .cursor_pointer()
            .bg(if is_open {
                theme::surface::SEGMENT_TRACK
            } else {
                work_surface::TRANSPARENT
            })
            .hover(|el| el.bg(theme::surface::SEGMENT_TRACK))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(13.0))
                    .text_color(theme::text::GHOST)
                    .child("+"),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(8.5))
                    .text_color(theme::text::PATH)
                    .child("\u{25be}"),
            )
            .child({
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.plus_button_bounds = bounds;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.plus_menu_open = !this.plus_menu_open;
                if this.plus_menu_open {
                    this.load_agent_rows(cx);
                }
                cx.notify();
            }))
    }

    /// The tab strip's `+` menu popover itself (`design_handoff_jerry_ade/revision/
    /// CHANGELOG.md`'s 2026-07-29 entry, change 4) - a real, absolutely-positioned scrim +
    /// panel painted as an unconditional child of [`Render::render`]'s root div, the exact same
    /// real overlay shape [`Self::render_palette`]'s own docs describe (a transparent, not
    /// dimmed, scrim here - the design has no full-window dimming for this smaller popover -
    /// whose own `on_click` closes the menu, with the panel itself stopping that click from
    /// bubbling up via `cx.stop_propagation()`). Positioned directly off
    /// [`Self::plus_button_bounds`] rather than a hand-computed pixel offset - see that field's
    /// own docs.
    ///
    /// Four real rows: *New terminal* ([`Self::new_session`] with [`SessionKind::Shell`]), *New
    /// agent pane* ([`Self::new_agent_pane`]), *Open file…* ([`Self::open_palette`], scoped to
    /// [`palette::PaletteScope::Files`] - see this row's own click handler, below), and *Next
    /// changed file* ([`Self::next_changed_file`]). The first, second, and fourth each dispatch
    /// the same real method their own real global keybinding does (see
    /// [`crate::default_key_bindings`]) and show that real binding's keycap; *Open file…* has no
    /// real global keybinding of its own (see that function's own docs for the real
    /// Ctrl+P/readline conflict that ruled one out) and so shows no keycap at all - a click-only
    /// row, not a decorative shortcut hint for one that doesn't actually fire. Every row's own
    /// click handler also closes the menu.
    pub(super) fn render_plus_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let macos = self.window_controls_style().is_macos();
        let bounds = self.plus_button_bounds;
        let (shadow_x, shadow_y, shadow_blur) = theme::shadow::PLUS_MENU;

        let resolved_kind = self.resolved_new_agent_kind();
        let (agent_fg, agent_bg) = work_surface::agent_tint(resolved_kind);
        let agent_initial = work_surface::agent_initial(resolved_kind);
        let agent_label = resolved_kind.label();
        let changed_count = self
            .current_diff()
            .map(|diff| diff.files.len())
            .unwrap_or(0);

        div()
            .id("plus-menu-scrim")
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .bg(work_surface::TRANSPARENT)
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.plus_menu_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("plus-menu-popover")
                    .absolute()
                    .left(bounds.origin.x + px(2.0))
                    .top(bounds.origin.y + bounds.size.height)
                    .w(theme::zone::PLUS_MENU_WIDTH)
                    .py(px(4.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .shadow(vec![BoxShadow::new(
                        shadow_x,
                        shadow_y,
                        gpui::black().opacity(0.55),
                    )
                    .blur_radius(shadow_blur)])
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .child(
                        render_plus_menu_row(
                            "\u{276f}",
                            theme::text::DIM,
                            theme::surface::CHIP_NEUTRAL,
                            "New terminal",
                            "in this worktree".to_string(),
                            keymap::resolve_combo("ctrl+shift+T", macos),
                        )
                        .on_click(cx.listener(
                            |this, _event: &ClickEvent, window, cx| {
                                this.new_session(SessionKind::Shell, window, cx);
                                this.plus_menu_open = false;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        render_plus_menu_row(
                            agent_initial,
                            agent_fg,
                            agent_bg,
                            "New agent pane",
                            agent_label.to_string(),
                            keymap::resolve_combo("mod+shift+N", macos),
                        )
                        .on_click(cx.listener(
                            |this, _event: &ClickEvent, _window, cx| {
                                this.new_agent_pane(cx);
                                this.plus_menu_open = false;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        render_plus_menu_row(
                            "@",
                            theme::palette::COMMAND_CHIP.0,
                            theme::palette::COMMAND_CHIP.1,
                            "Open file\u{2026}",
                            "search this worktree".to_string(),
                            // No real keycap - see `Self::render_plus_menu`'s own docs for why
                            // this row has no real global keybinding to show a shortcut for
                            // (`render_keycap_row` renders nothing for an empty slice, the same
                            // "no real binding behind it" convention `render_hint_pair`'s own
                            // docs establish).
                            Vec::new(),
                        )
                        .on_click(cx.listener(
                            |this, _event: &ClickEvent, window, cx| {
                                this.plus_menu_open = false;
                                this.open_palette(window, cx);
                                // Real files-only scope: `palette::PaletteScope::Files` already
                                // exists, with a real segment, `@` prefix, and real file results
                                // - `open_palette` itself always resets `palette_scope` back to
                                // `PaletteScope::default()` (`All`), so this has to be set right
                                // after it returns, not before.
                                this.palette_scope = palette::PaletteScope::Files;
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        render_plus_menu_row(
                            "]",
                            theme::text::DIM,
                            theme::surface::CHIP_NEUTRAL,
                            "Next changed file",
                            format!("{changed_count} changed"),
                            keymap::resolve_combo("]", macos),
                        )
                        .on_click(cx.listener(
                            |this, _event: &ClickEvent, window, cx| {
                                this.plus_menu_open = false;
                                this.next_changed_file(window, cx);
                            },
                        )),
                    ),
            )
    }

    /// Which real agent kind the `+` menu's "New agent pane" row would spawn right now, and
    /// shows as its own sub-label/chip - the first [`settings::AGENT_KINDS`] entry a real
    /// `$PATH` search ([`Self::agent_rows`], refreshed on menu open - see
    /// [`Self::render_tab_strip_plus`]'s docs) confirms is actually installed, or
    /// `AGENT_KINDS[0]` if none are (or `agent_rows` hasn't been populated yet - e.g. a menu
    /// opened before its own `load_agent_rows` call resolves). [`Self::new_agent_pane`] runs the
    /// same real detection independently, off the foreground thread, at the moment it actually
    /// spawns - this is a *display* best-effort matching it as closely as this render's already-
    /// cached data allows, not the source of truth for what gets spawned.
    pub(super) fn resolved_new_agent_kind(&self) -> SessionKind {
        settings::AGENT_KINDS
            .into_iter()
            .find(|kind| {
                self.agent_rows
                    .iter()
                    .any(|row| row.kind == *kind && row.is_ready())
            })
            .unwrap_or(settings::AGENT_KINDS[0])
    }

    /// The `+` menu's real "New agent pane" action (`ctrl-shift-T`'s sibling,
    /// `secondary-shift-n`) - spawns the first [`settings::AGENT_KINDS`] entry a real,
    /// background `$PATH` search (`pty_core::resolve_on_path`, the same real search
    /// [`Self::load_agent_rows`] runs for the Settings › Agents page) confirms is actually
    /// installed, mirroring [`Self::load_agent_rows`]'s own "gather on foreground, search on
    /// background" shape rather than blocking the click on a real filesystem walk.
    ///
    /// If *no* configured agent is installed, this does not silently no-op - it spawns
    /// `AGENT_KINDS[0]` anyway, exactly like the session toolbar's own `+ claude`/`+ codex`
    /// buttons already do when that binary isn't on `$PATH` (`Self::render_session_toolbar`'s
    /// docs): the process genuinely fails to spawn and a real, non-panicking spawn error shows
    /// in the new tab (`TerminalPane::spawn_error`), the same honest failure path this app
    /// already has, not a second, different "nothing to do" behavior invented just for this
    /// button.
    pub(super) fn new_agent_pane(&mut self, cx: &mut Context<Self>) {
        let cwd = self.active_session_cwd();
        let task = cx.spawn(async move |this, cx| {
            let installed = cx
                .background_executor()
                .spawn(async move {
                    settings::AGENT_KINDS.into_iter().find(|kind| {
                        kind.agent_binary_name()
                            .and_then(pty_core::resolve_on_path)
                            .is_some()
                    })
                })
                .await;
            // Needs real `Window` access to move focus onto the newly spawned session's own
            // pane (`Self::focus_newly_spawned_session`'s own docs) - `Entity::update_in` gets
            // it anyway, the same real `AsyncApp::with_window` mechanism
            // `Self::navigate_to_definition`'s own background completion already relies on for
            // the same reason.
            let _ = this.update_in(cx, |this, window, cx| {
                let kind = installed.unwrap_or(settings::AGENT_KINDS[0]);
                this.sessions.spawn(kind, cwd, cx);
                this.focus_newly_spawned_session(window, cx);
                this.prune_confirm_armed = false;
                cx.notify();
            });
        });
        // A `Vec`, not a single `Option` slot - see `Self::_new_agent_pane_task`'s own docs for
        // the real, live bug a single slot had: two rapid "New agent pane" clicks before the
        // first click's background `$PATH` search resolved used to drop (and so immediately
        // cancel, per GPUI's real "dropping a `Task` cancels it immediately" semantics) the
        // first click's task the instant the second one was assigned here, silently spawning
        // only one session for two real clicks. Mirrors `Self::_lsp_tasks`/
        // `Self::_goto_definition_tasks`'s own "independent operations, dropping an unrelated
        // one would cancel it" shape - pruned of already-finished entries before each push.
        self._new_agent_pane_task.retain(|task| !task.is_ready());
        self._new_agent_pane_task.push(task);
    }

    /// The `+` menu's real "Next changed file" action (`]`) - given the currently active file
    /// tab (or, if none is active, the first entry of the real Changes list -
    /// `design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29 entry, change 4), opens the
    /// next real changed file after it as a tab, **wrapping around** to the first changed file
    /// once the last one is passed (a documented choice over stopping at the end - a repeatable
    /// `]` press cycles through every changed file indefinitely, matching how the session-jump
    /// keycaps and palette arrow keys already treat "next"/"previous" as a real loop rather than
    /// a dead end). If the active file isn't itself a changed file (e.g. opened from the file
    /// tree, not Changes), or nothing is active at all, this opens the *first* real changed file,
    /// since there's no real "current position" in the changed-file list to advance from
    /// otherwise. A real no-op when there is no loaded diff, or it has no changed files.
    pub(super) fn next_changed_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next_path = {
            let Some(diff) = self.current_diff() else {
                return;
            };
            if diff.files.is_empty() {
                return;
            }
            let current_index = self
                .open_change
                .as_ref()
                .and_then(|active| diff.files.iter().position(|file| &file.path == active));
            let next_index = match current_index {
                Some(index) => (index + 1) % diff.files.len(),
                None => 0,
            };
            diff.files[next_index].path.clone()
        };
        self.open_change_diff(next_path, window, cx);
    }

    /// The tab strip's real session-jump keycaps (`secondary-1`..`secondary-8`) - jumps to the
    /// session at 1-indexed `position` in the same real, live order [`Self::render_tab_strip`]
    /// already iterates (`Self::sessions.iter()`), via [`Self::select_session`] (the exact same
    /// real switch a session tab's own click performs). A real no-op if fewer than `position`
    /// sessions currently exist - this closes a real "looks real but isn't" gap: before this,
    /// the tab strip rendered a `mod`/`1…8` keycap pair with no matching `secondary-1`..
    /// `secondary-8` binding anywhere (`crate::keymap::resolve_combo`'s own docs call `"1…8"` a
    /// placeholder token it passes through unresolved), so every one of those keycaps was purely
    /// decorative.
    pub(super) fn jump_to_session_at(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = position
            .checked_sub(1)
            .and_then(|index| self.sessions.iter().nth(index))
            .map(|session| session.id)
        else {
            return;
        };
        self.select_session(id, window, cx);
    }

    /// [`NewTerminal`]'s real, bound `ctrl-shift-T` action handler - the `+` menu's "New
    /// terminal" row's own keybinding, spawning a real [`SessionKind::Shell`] session exactly
    /// like the row's own click handler does.
    pub(super) fn handle_new_terminal_action(
        &mut self,
        _action: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, window, cx);
    }

    /// [`NewAgentPane`]'s real, bound `secondary-shift-n` action handler - see
    /// [`Self::new_agent_pane`]'s docs.
    pub(super) fn handle_new_agent_pane_action(
        &mut self,
        _action: &NewAgentPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_pane(cx);
    }

    /// [`NextChangedFile`]'s real, bound `]` action handler - see [`Self::next_changed_file`]'s
    /// docs.
    pub(super) fn handle_next_changed_file_action(
        &mut self,
        _action: &NextChangedFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.next_changed_file(window, cx);
    }

    session_jump_action_handler!(handle_jump_to_session_1_action, JumpToSession1, 1);
    session_jump_action_handler!(handle_jump_to_session_2_action, JumpToSession2, 2);
    session_jump_action_handler!(handle_jump_to_session_3_action, JumpToSession3, 3);
    session_jump_action_handler!(handle_jump_to_session_4_action, JumpToSession4, 4);
    session_jump_action_handler!(handle_jump_to_session_5_action, JumpToSession5, 5);
    session_jump_action_handler!(handle_jump_to_session_6_action, JumpToSession6, 6);
    session_jump_action_handler!(handle_jump_to_session_7_action, JumpToSession7, 7);
    session_jump_action_handler!(handle_jump_to_session_8_action, JumpToSession8, 8);

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
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        this.select_session(id, window, cx);
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
                            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.close_session(id, window, cx);
                                cx.notify();
                            })),
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
            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                this.archive_session(id, window, cx);
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
                    cx.listener(move |this, _event: &ClickEvent, window, cx| match kind {
                        work_surface::ActionKind::Interrupt => this.interrupt_session(id, cx),
                        work_surface::ActionKind::OpenTerminal => {
                            this.open_companion_terminal(cwd.clone(), window, cx)
                        }
                        work_surface::ActionKind::Respawn => this.respawn_session(id, window, cx),
                        work_surface::ActionKind::Archive => this.archive_session(id, window, cx),
                        work_surface::ActionKind::Unimplemented => {}
                    }),
                );
        } else {
            button = button.cursor_default();
        }

        button
    }

    /// The centre pane's content - `design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29
    /// entry, change 4: the unified tab strip ([`Self::render_tab_strip`]) always renders first,
    /// as a real sibling above whichever body follows, rather than being something the file-view
    /// code path bypasses entirely the way it used to (this method used to `return` a whole
    /// separate, tab-strip-less subtree the instant a file was open). Below it: either the real
    /// active file tab's own Surface C (`Self::render_code_surface`) if [`Self::open_change`]
    /// names one, or the real active session's toolbar/context-bar/pty otherwise - the exact
    /// same two bodies this method already rendered, just both now stacked under one shared tab
    /// strip instead of the file body replacing it.
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
        let mut surface = div()
            .id("work-surface")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme::surface::CENTER)
            .child(self.render_tab_strip(cx));

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
                let body = self.render_code_surface(&open_path, diff_file.as_ref(), cx);
                self.open_diff_file_cache = diff_file;
                return surface.child(body).into_any_element();
            }
            self.open_diff_file_cache = diff_file;
        }

        surface = surface.child(self.render_session_toolbar(cx));

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

/// One row of the tab strip's `+` menu popover (`design_handoff_jerry_ade/revision/
/// CHANGELOG.md`'s 2026-07-29 entry, change 4: "29-high rows of chip · label (nowrap) · dim sub
/// · hint keycaps"). Returns the row with no click handler wired yet - `Self::render_plus_menu`
/// attaches each row's own real action via `.on_click(cx.listener(...))` after the fact, since a
/// free function has no `Context<AdeApp>` of its own to build a listener from. `keys` is already
/// platform-resolved (`crate::keymap::resolve_combo`'s output), rendered through the existing
/// [`render_keycap_row`] at [`KeycapSize::Hint`] - reused as-is rather than a new one-off keycap
/// renderer, per this project's own "don't hand-roll new keycap rendering" convention; the
/// mockup's own popover row happens to use a 3px keycap gap where [`KeycapSize::Hint`]'s
/// established gap is 2px (see that type's own docs for the real, deliberate precedent this
/// mirrors instead), a one-pixel difference accepted in favor of reusing the real, shared widget.
fn render_plus_menu_row(
    chip_glyph: &'static str,
    chip_fg: gpui::Rgba,
    chip_bg: gpui::Rgba,
    label: &'static str,
    sub: String,
    keys: Vec<String>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(format!("plus-menu-row-{label}"))
        .flex()
        .items_center()
        .gap(px(9.0))
        .h(theme::band::PLUS_MENU_ROW)
        .px(px(10.0))
        .cursor_pointer()
        .hover(|el| el.bg(theme::surface::PLUS_MENU_ROW_HOVER))
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .h(px(14.0))
                .rounded(theme::radius::CHIP)
                .flex()
                .items_center()
                .justify_center()
                .bg(chip_bg)
                .font(font(theme::font::MONO))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(8.0))
                .text_color(chip_fg)
                .child(chip_glyph),
        )
        .child(
            div()
                .flex_none()
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(11.5))
                .text_color(theme::text::HEADING)
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .truncate()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::FAINTER)
                .child(sub),
        )
        .child(render_keycap_row(&keys, KeycapSize::Hint))
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
