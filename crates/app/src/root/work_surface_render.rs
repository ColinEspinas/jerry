use super::*;
use crate::root::widgets::{
    render_action_keycap_row, render_env_chip, render_hint_pair, render_keycap_row, KeycapSize,
};

/// Defines one `JumpToSessionN` action handler forwarding a literal position to
/// [`AdeApp::jump_to_session_at`]. Each `actions!`-generated struct is a distinct action type
/// with no positional data, so GPUI needs one `on_action` handler per keystroke regardless; this
/// macro just keeps the eight near-identical bodies from drifting from each other.
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
        self.sessions.spawn(
            kind,
            cwd,
            self.settings.appearance.terminal_font_size,
            window,
            cx,
        );
        self.focus_newly_spawned_session(window, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Moves focus onto the session [`Sessions::spawn`] just made active - but only when a file
    /// tab isn't showing instead ([`Self::render_center_pane`] renders the file tab in that
    /// case, not a session's `TerminalPane`), since focusing a session's pane while that's true
    /// would point `Window::focus` at a node nothing in the rendered tree tracks.
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

    /// Activates session `id`'s tab and, if it maps to a currently-listed worktree, also selects
    /// that worktree, keeping the file tree/diff sidebar in sync with the session just clicked
    /// (the sidebar is still driven by [`Self::selected`] - a `focused_session`-driven Zone 2/3
    /// hasn't been rebuilt yet). If a file tab was active, this deactivates it
    /// (`Self::open_change = None`, without closing it - it stays in [`Self::open_files`]) and
    /// restores focus onto the session's pane via [`restore_focus`].
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
            restore_focus(&self.sessions, &mut self.code_focus, window, cx);
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

    /// Derives the [`Status`] for a live session - the single source of truth both
    /// [`Self::build_session_rows`] (the rail) and the work surface (status pill, pane header/
    /// footer) read, so the rail and the work surface can never disagree about a session's
    /// status.
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
            // A process that never started still counts as a failure, even though it has no
            // `ExitStatus` to report.
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

    /// The context bar's and idle-status footer's `Archive` action - closes the tab via
    /// [`Self::close_session`] (see that method's docs for why every close path must go through
    /// it rather than `Sessions::close` directly).
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

    /// Closes session `id`'s tab (`Sessions::close` tears down its child process and moves focus
    /// onto whichever session becomes active) and, if `id` is the session whose `Merge` click
    /// started [`Self::merge_flow`], cleans that up too (see
    /// [`Self::clear_merge_flow_for_closed_session`]).
    ///
    /// Every close path - [`Self::archive_session`], [`Self::respawn_session`]'s
    /// close-then-respawn, and the tab strip's own `×` - must go through this function rather
    /// than `Sessions::close` directly: previously only `archive_session` cleared `merge_flow`,
    /// so archiving (or retrying) a mid-merge session left `merge_flow.session_id` pointing at a
    /// session that no longer existed, permanently disabling the `Merge` button for every
    /// session (`Self::render_merge_button`'s disabled check never cleared).
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

    /// The surface footer's `Interrupt ⌃C` action - sends `Ctrl-C` to the session's pty via
    /// `TerminalPane::interrupt`.
    pub(super) fn interrupt_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let pane = session.pane.clone();
        pane.update(cx, |pane, cx| pane.interrupt(cx));
    }

    /// The surface footer's `Retry ⌘R` (failed sessions) / `Resume ⌘⏎` (idle sessions) action.
    /// This app has no saved-session resumability to resume *from* (see
    /// `crate::work_surface::pty_state_label`'s docs), so the honest equivalent is: close this
    /// tab, then spawn a fresh session of the same kind into the same worktree - not literally
    /// "resume where it left off" (`crate::work_surface::ActionKind::Respawn`'s docs name this
    /// trade-off).
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
        self.sessions.spawn(
            kind,
            cwd,
            self.settings.appearance.terminal_font_size,
            window,
            cx,
        );
        self.focus_newly_spawned_session(window, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// The surface footer's `Open terminal` action - selects an already-open `Shell` session in
    /// the same worktree, or spawns one if none exists. Each session is its own independent tab/
    /// process (`crate::sessions`'s module docs), so "open terminal" just means "get me a shell
    /// in this worktree", the same capability as the rail's "+ New Shell" button.
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
                self.sessions.spawn(
                    SessionKind::Shell,
                    cwd,
                    self.settings.appearance.terminal_font_size,
                    window,
                    cx,
                );
                self.focus_newly_spawned_session(window, cx);
                self.prune_confirm_armed = false;
                cx.notify();
            }
        }
    }

    /// The "+ shell" / "+ claude" / "+ codex" row above the tab strip, spawning a session into
    /// `active_session_cwd()` and showing which worktree that resolves to. Not part of the
    /// mockup (whose rail/tab-strip `+` only spawn one default kind - per-kind selection lives
    /// in the design's Settings › Agents page, out of scope here) - kept because without it this
    /// app would have no way to start a `claude`/`codex` session at all.
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

    /// The tab strip: one [`render_session_tab`] per live [`crate::sessions::Session`], followed
    /// by one [`Self::render_file_tab`] per entry of [`Self::open_files`] in that `Vec`'s order,
    /// then the `+` menu button ([`Self::render_tab_strip_plus`]) and right-aligned session-jump
    /// keycaps.
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

        // Only show keycaps for sessions that actually exist, capped at 8 since
        // `secondary-1`..`secondary-8` are the only ones bound (`crate::default_key_bindings`).
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

    /// A file tab: language chip (`file_tree::lang_chip_for_name`, dimmed via
    /// `work_surface::file_tab_chip_colors` when inactive), file name, and a close hit box.
    /// Clicking the body activates the tab ([`Self::activate_file_tab`]); clicking `×` closes it
    /// ([`Self::close_file_tab`]) and stops propagation so it doesn't also activate (the same
    /// pattern [`render_session_tab`]'s close button uses). Shares active/inactive bg/underline/
    /// label colours with session tabs (`work_surface::tab_colors`).
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
                            .text_size(self.ui_text_size(11.0))
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

    /// The tab strip's `+` menu button - toggles [`Self::plus_menu_open`] (unconditionally
    /// spawning a shell is the rail's separate `+` -
    /// [`crate::root::rail_render::render_new_session_button`]). A `gpui::canvas` child captures
    /// this button's painted bounds into [`Self::plus_button_bounds`] every render, which
    /// [`Self::render_plus_menu`] positions the popover off of. Opening the menu also refreshes
    /// [`Self::load_agent_rows`], so the "New agent pane" row's sub-label
    /// ([`Self::resolved_new_agent_kind`]) reflects a reasonably fresh `$PATH` search rather than
    /// a possibly-empty cached snapshot.
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

    /// The tab strip's `+` menu popover: an absolutely-positioned scrim + panel, the same overlay
    /// shape [`Self::render_palette`] uses (transparent, not dimmed - the design has no
    /// full-window dimming for this smaller popover). The scrim's `on_click` closes the menu;
    /// the panel stops that click from bubbling up (`cx.stop_propagation()`). Positioned off
    /// [`Self::plus_button_bounds`].
    ///
    /// Four rows: *New terminal* ([`Self::new_session`] with [`SessionKind::Shell`]), *New agent
    /// pane* ([`Self::new_agent_pane`]), *Open file…* ([`Self::open_palette`], scoped to
    /// [`palette::PaletteScope::Files`]), and *Next changed file* ([`Self::next_changed_file`]).
    /// The first, second, and fourth each dispatch the same method their own global keybinding
    /// does (`crate::default_key_bindings`) and show that binding's keycap; *Open file…* has no
    /// global keybinding of its own (a Ctrl+P/readline conflict ruled one out - see that
    /// function's docs) and so shows no keycap. Every row's click handler also closes the menu.
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
                            // No keycap: this row has no global keybinding (see the function
                            // docs above), and `render_keycap_row` renders nothing for `&[]`.
                            Vec::new(),
                        )
                        .on_click(cx.listener(
                            |this, _event: &ClickEvent, window, cx| {
                                this.plus_menu_open = false;
                                this.open_palette(window, cx);
                                // `open_palette` always resets `palette_scope` to
                                // `PaletteScope::default()`, so this must be set after it
                                // returns, not before.
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

    /// Which agent kind the `+` menu's "New agent pane" row would spawn right now: the first
    /// [`settings::AGENT_KINDS`] entry [`Self::agent_rows`] (refreshed on menu open) confirms is
    /// installed, or `AGENT_KINDS[0]` if none are (or `agent_rows` hasn't been populated yet).
    /// Display-only - [`Self::new_agent_pane`] runs its own detection independently, off the
    /// foreground thread, at the moment it actually spawns.
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

    /// The `+` menu's "New agent pane" action (`secondary-shift-n`) - spawns the first
    /// [`settings::AGENT_KINDS`] entry a background `$PATH` search
    /// (`pty_core::resolve_on_path`, the same search [`Self::load_agent_rows`] runs) confirms is
    /// installed, rather than blocking the click on a filesystem walk.
    ///
    /// If no configured agent is installed, this spawns `AGENT_KINDS[0]` anyway, same as the
    /// session toolbar's `+ claude`/`+ codex` buttons when that binary isn't on `$PATH`: the
    /// process fails to spawn and a non-panicking spawn error shows in the new tab
    /// (`TerminalPane::spawn_error`).
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
            // Needs `Window` access to move focus onto the newly spawned session's pane
            // (`Self::focus_newly_spawned_session`) - `Entity::update_in` provides it.
            let _ = this.update_in(cx, |this, window, cx| {
                let kind = installed.unwrap_or(settings::AGENT_KINDS[0]);
                this.sessions.spawn(
                    kind,
                    cwd,
                    this.settings.appearance.terminal_font_size,
                    window,
                    cx,
                );
                this.focus_newly_spawned_session(window, cx);
                this.prune_confirm_armed = false;
                cx.notify();
            });
        });
        // A `TaskPool`, not a single `Option` slot: two rapid clicks before the first click's
        // `$PATH` search resolves must not drop (and so cancel, per GPUI's "dropping a `Task`
        // cancels it" semantics) the first click's task when the second is assigned.
        self._new_agent_pane_task.push(task);
    }

    /// The `+` menu's "Next changed file" action (`]`) - opens the next changed file after the
    /// active file tab as a tab, wrapping around to the first once the last is passed (so a
    /// repeated `]` press cycles indefinitely, matching how the session-jump keycaps and palette
    /// arrow keys already treat "next"/"previous"). If the active file isn't itself a changed
    /// file, or nothing is active, this opens the first changed file. No-op if there's no loaded
    /// diff, or it has no changed files.
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

    /// The tab strip's session-jump keycaps (`secondary-1`..`secondary-8`) - jumps to the
    /// session at 1-indexed `position` in the same order [`Self::render_tab_strip`] iterates
    /// (`Self::sessions.iter()`), via [`Self::select_session`]. No-op if fewer than `position`
    /// sessions currently exist.
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

    /// [`NewTerminal`]'s `ctrl-shift-T` action handler - the `+` menu's "New terminal" row's own
    /// keybinding, spawning a [`SessionKind::Shell`] session like the row's click handler does.
    pub(super) fn handle_new_terminal_action(
        &mut self,
        _action: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, window, cx);
    }

    /// [`NewAgentPane`]'s `secondary-shift-n` action handler - see [`Self::new_agent_pane`].
    pub(super) fn handle_new_agent_pane_action(
        &mut self,
        _action: &NewAgentPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_pane(cx);
    }

    /// [`NextChangedFile`]'s `]` action handler - see [`Self::next_changed_file`].
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

    /// One tab: a 14×14 kind chip, the label (resolved binary name for an agent CLI tab, or
    /// `terminal` for a shell tab), and a `×` that closes it (`Sessions::close`, tearing down
    /// the process). Split into a `flex_1` clickable content row plus a `flex_none` 1px
    /// underline bar, rather than a single div with two differently-coloured borders, because
    /// GPUI's `Style::border_color` is one colour for every edge
    /// (`vendor/zed/crates/gpui/src/style.rs`) - it can't give the right border (always
    /// `theme::border::INNER`) and the active/inactive-dependent underline two different colours
    /// on the same div.
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
                            .text_size(self.ui_text_size(if is_mono { 11.0 } else { 11.5 }))
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

    /// The session context bar: agent badge/name, a divider, branch, the worktree path (the one
    /// flexible, ellipsising child - every other child is `flex_none` and non-wrapping, so the
    /// bar never wraps when the centre narrows), a status pill, and `Merge`/`Archive`.
    pub(super) fn render_session_context_bar(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.session_status(session, cx);
        let (agent_fg, agent_bg) = work_surface::agent_tint(session.kind);
        let agent_initial = work_surface::agent_initial(session.kind);
        // `SessionKind` only tracks which CLI binary is running, not which model it's
        // configured to use, so `session.kind.label()` ("Claude"/"Codex"/"Shell") is the
        // closest honest substitute for a model name this app never actually observes.
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

    /// The context bar's `Merge` button - starts [`Self::start_merge`]. Disabled (dimmed,
    /// non-interactive) whenever any merge flow is already active, own session or not (only one
    /// runs at a time - see [`Self::start_merge`]'s docs), and shows `Merging…` in place of
    /// `Merge` while this session's own attempt is the one running.
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

    /// The context bar's `Archive` button - see [`Self::archive_session`].
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

    /// Surface A/B's shared header: the resolved program label (this app has no saved-session
    /// resumability, so there's no resume argument to show alongside it), a `Shell` session's
    /// cwd, and a hint row (`mod + click a path to open it`, and a click-only `clear`) rendered
    /// for every session kind - `TerminalPane` behaves identically for shell and agent sessions
    /// (see its module docs), so link-click and `clear` are exactly as real for a `Claude`/
    /// `Codex` panic frame as for a shell prompt.
    ///
    /// A `Shell` label gets a ` · wsl` suffix when running inside WSL (`crate::env_info::is_wsl`).
    /// The design's third hint, `split`, is deliberately not rendered - this app has no
    /// pane-splitting feature, and this codebase omits hints for features that don't exist
    /// rather than showing a decorative keycap for one (the same precedent
    /// [`Self::render_plus_menu`]'s "Open file…" row sets).
    ///
    /// A `Claude`/`Codex` session's pid is shown once, in the info footer below
    /// ([`Self::render_pty_info_footer`]) - not duplicated here.
    ///
    /// `clear` is click-only, not a global keybinding, even though the design shows `mod+K`:
    /// `Ctrl+K` is a standard readline binding (`kill-line`) every focused shell relies on, and
    /// `"mod"` resolves to plain `Ctrl` on Linux/Windows (`crate::keymap`'s docs) - binding it
    /// globally would repeat the same "app-level shortcut steals terminal input" class of bug
    /// `crate::default_key_bindings` already documents for `secondary-p`/Ctrl+P. Zed's own
    /// keymaps confirm this isn't overcaution: `terminal::Clear` is bound to `ctrl-shift-l` on
    /// Linux/Windows and reserved for `cmd-k` on macOS alone, where a platform-modified keystroke
    /// never reaches the pty in the first place (`crate::terminal_pane::keystroke_to_bytes`).
    pub(super) fn render_pty_header(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = session.pane.read(cx);
        let program_label = pane.program_label();
        let is_running = pane.is_running();
        let exit_code = pane.exit_status().map(|status| status.exit_code());
        let status_value = self.session_status(session, cx);
        let state_label = work_surface::pty_state_label(is_running, status_value, exit_code);
        let is_wsl_shell = session.kind == SessionKind::Shell && env_info::is_wsl();
        let label_text = if is_wsl_shell {
            format!("{program_label} \u{b7} wsl")
        } else {
            program_label
        };

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
                    .child(label_text),
            );

        let header = match session.kind {
            SessionKind::Shell => header.child(
                div()
                    .flex_none()
                    .max_w(px(280.0))
                    .overflow_hidden()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child(session.cwd.display().to_string()),
            ),
            // No per-kind header content for an agent session - pid is shown once, in the info
            // footer below.
            SessionKind::Claude | SessionKind::Codex => header,
        };

        let macos = self.window_controls_style().is_macos();
        let pane_entity = session.pane.clone();
        let header = header.child(div().flex_1()).child(
            div()
                .id("pty-header-hints")
                .flex()
                .items_center()
                .gap(px(11.0))
                .child(render_hint_pair(
                    &keymap::resolve_combo("mod", macos),
                    "click a path to open it",
                ))
                .child(
                    div()
                        .id("pty-header-clear")
                        .cursor_pointer()
                        .rounded(theme::radius::CHIP)
                        .px(px(3.0))
                        .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                        .child(render_hint_pair(&[], "clear"))
                        .on_click(cx.listener(move |_this, _event: &ClickEvent, _window, cx| {
                            pane_entity.update(cx, |pane, cx| pane.clear(cx));
                        })),
                ),
        );

        header.child(
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::HINT)
                .child(state_label),
        )
    }

    /// The terminal pane's info footer: pid, grid dimensions, the environment chip, and a hint
    /// about file:line references. Rendered for every session kind - `TerminalPane` is the same
    /// component behind a `Shell` tab and a `Claude`/`Codex` tab (see that module's docs), so pid
    /// and grid dimensions are equally meaningful for either. Distinct from, and rendered
    /// alongside, [`Self::render_pty_footer`] - the session-level Interrupt/Retry/Archive action
    /// footer.
    pub(super) fn render_pty_info_footer(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pane = session.pane.read(cx);
        let pid = pane.pid();
        let (cols, rows) = pane.grid_dimensions();

        let divider = || {
            div()
                .flex_none()
                .w(px(1.0))
                .h(px(11.0))
                .bg(theme::border::DIVIDER)
        };
        let mono_text = |text: String| {
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::PATH)
                .child(text)
        };

        let mut footer = div()
            .id("pty-info-footer")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::band::PTY_INFO_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER);

        if let Some(pid) = pid {
            footer = footer
                .child(mono_text(format!("pid {pid}")))
                .child(divider());
        }
        footer = footer
            .child(mono_text(format!("{cols}\u{d7}{rows}")))
            .child(divider())
            .child(render_env_chip());

        footer.child(div().flex_1()).child(
            div()
                .flex_none()
                .font(font(theme::font::SANS))
                .text_size(px(10.0))
                .text_color(theme::text::HINT)
                .child("file:line references open in a tab"),
        )
    }

    /// Surface A/B's shared footer: the `JERRY` wordmark plus git-level actions appropriate to
    /// the session's status. See `crate::work_surface::footer_actions`/
    /// [`Self::render_footer_action_button`] for which actions are implemented vs. disabled.
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
            // A live, merely-idle shell has nothing to "resume" (see
            // `crate::work_surface::ActionKind::Respawn`'s docs) - disable it in that case
            // rather than letting a click spawn a redundant duplicate session.
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

    /// One footer action button - interactive (`cursor_pointer`, hover, `on_click` dispatch on
    /// `action.kind`) when `enabled`, otherwise dimmed with no cursor/hover/click at all - never
    /// a button that looks clickable but silently does nothing.
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
                // A disabled action must never keep its full-colour fill - that would make an
                // inert button look as clickable as a real one (a disabled "Resume" was once
                // found rendering with a solid fill next to a working "Archive"). The design has
                // no separate disabled-background token, so falling back to `TRANSPARENT` lets
                // the footer's own background show through instead.
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

    /// The centre pane's content: the unified tab strip ([`Self::render_tab_strip`]) always
    /// renders first, above either the active file tab's Surface C
    /// (`Self::render_code_surface`) if [`Self::open_change`] names one, or the active session's
    /// toolbar/context-bar/pty otherwise.
    ///
    /// A file opened via a Changes-row click (`Self::open_change_diff`) always has a `DiffFile`
    /// to show; one opened via the Files tree (`Self::open_file_view`) may not, in which case
    /// `diff_file` below is `None` and `Self::render_code_surface` shows the File view
    /// unconditionally.
    ///
    /// The terminal-surface fallback's root div keeps `.min_w_0()`: GPUI's flexbox gives a flex
    /// item's minimum width its content's intrinsic width by default, so an unbroken wide
    /// terminal row could otherwise grow this pane past its `flex_1` share and push the
    /// fixed-width right sidebar off screen (`.min_w_0()` zeroes that automatic minimum - see
    /// `vendor/zed/crates/agent_ui/src/agent_panel.rs`'s own `.flex_1().min_w_0()` use for the
    /// same overflow guard).
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
            // `open_diff_file_cache` already holds the up-to-date `DiffFile` for `open_path`
            // (kept fresh by `Self::refresh_open_diff_file_cache`). Taking it out (an `O(1)`
            // pointer swap, not a clone) is required because `Self::render_code_surface` needs
            // `&mut self`, which a live `&DiffFile` borrow from `self` can't coexist with.
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
                        .child(self.render_pty_info_footer(session, cx))
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

/// One row of the tab strip's `+` menu popover: chip, label, dim sub-label, and hint keycaps.
/// Returns the row with no click handler wired yet - [`Self::render_plus_menu`] attaches each
/// row's own action via `.on_click(cx.listener(...))` after the fact, since a free function has
/// no `Context<AdeApp>` to build a listener from. `keys` is already platform-resolved
/// (`crate::keymap::resolve_combo`'s output), rendered via [`render_keycap_row`] at
/// [`KeycapSize::Hint`].
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

/// The tab strip's 14×14 kind chip - a `❯` glyph tinted with the session's agent colour for
/// agent CLI tabs, or a pane glyph (a bar plus a prompt mark) for terminal tabs. Turns
/// `work_surface::tab_chip_kind`/`tab_chip_colors`'s mapping into GPUI elements; no
/// chip-selection logic lives here.
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

/// The session context bar's status pill: a coloured dot plus label in the status colour.
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
