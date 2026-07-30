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
        self.discard_confirm_armed = None;
        cx.notify();
    }

    /// Moves focus onto the session [`Sessions::spawn`] just made active - but only when neither
    /// a file tab ([`Self::render_center_pane`] renders the file tab in that case, not a
    /// session's `TerminalPane`) nor Settings ([`Self::settings_open`] - Settings replaces the
    /// entire workspace body, per `crate::root::mod`'s own docs, so no session's pane is
    /// rendered anywhere while it's showing) is occupying the centre pane instead, since focusing
    /// a session's pane while either is true would point `Window::focus` at a node nothing in the
    /// rendered tree tracks. Reachable with Settings open via the title bar's Session menu (New
    /// Terminal/New Agent Pane), which is an unconditional sibling of the Settings/workspace-body
    /// swap.
    pub(super) fn focus_newly_spawned_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_change.is_none() && !self.settings_open {
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
        self.discard_confirm_armed = None;
        if self.open_change.is_some() {
            self.open_change = None;
            self.refresh_open_diff_file_cache();
            self.hover = None;
            // See `crate::root::code_surface::AdeApp::open_change_diff`'s identical
            // `dismiss_completions()` call for why (Revision R8.5b audit finding 3).
            self.dismiss_completions();
            if self.settings_open {
                // Settings is showing over the whole workspace body right now (reachable here
                // via the title bar's Session menu cycle rows/Archive Session, unconditional
                // siblings of the Settings/workspace-body swap) - real focus already correctly
                // lives on `settings_focus_handle`. Discard the captured pre-file-tab target
                // rather than restoring it onto a session pane `Self::render_settings` isn't
                // drawing, mirroring `Self::close_palette`'s identical Settings-aware branch
                // (`self.palette_focus.clear()`).
                self.code_focus.clear();
            } else {
                restore_focus(&self.sessions, &mut self.code_focus, window, cx);
            }
        }
        let cwd = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| session.cwd.clone());
        if let Some(cwd) = cwd {
            if let Some(index) = self.worktrees.iter().position(|item| item.path == cwd) {
                if self.selected != Some(index) {
                    self.select_worktree(index, window, cx);
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
        self.discard_confirm_armed = None;
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
    ///
    /// Tells `Sessions::close` to skip its own focus move whenever the centre pane isn't
    /// actually showing a session's pane right now - a file tab is open, *or* Settings has
    /// replaced the whole workspace body (`Self::settings_open`, see the title bar's Session
    /// menu docs - Archive Session is reachable from there while Settings is showing, and
    /// moving focus onto a pane `Self::render_settings` isn't drawing would dangle it exactly
    /// like the file-tab case this guard already covered).
    ///
    /// If closing `id` leaves its worktree with no session at all (and no file tab either), real
    /// keyboard focus falls back onto [`Self::filter_focus_handle`] - the same fallback
    /// [`Self::select_worktree`] uses for the identical "nothing left to focus" case - so
    /// `Window::focus` never stays pointed at the just-`shutdown()`, no-longer-rendered pane.
    pub(super) fn close_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let skip_focus_move = self.open_change.is_some() || self.settings_open;
        self.sessions.close(id, skip_focus_move, window, cx);
        if self
            .merge_flow
            .as_ref()
            .is_some_and(|flow| flow.session_id == id)
        {
            self.clear_merge_flow_for_closed_session(cx);
        }
        if self.sessions.active_id().is_none() && self.open_change.is_none() && !self.settings_open
        {
            window.focus(&self.filter_focus_handle, cx);
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
        self.discard_confirm_armed = None;
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
                self.discard_confirm_armed = None;
                cx.notify();
            }
        }
    }

    /// Every session open in the *currently selected* worktree (`Self::active_session_cwd`), in
    /// creation order - the real per-worktree tab-strip filter (`crate::sessions::Sessions::
    /// iter_for_cwd`) both [`Self::render_tab_strip`] and [`Self::session_jump_keys`]/
    /// [`Self::jump_to_session_at`] share, so the tabs shown and the tabs a jump keycap can
    /// reach can never disagree.
    pub(super) fn current_worktree_sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter_for_cwd(self.active_session_cwd())
    }

    /// The tab strip: one [`render_session_tab`] per session open in the *currently selected*
    /// worktree (`Self::current_worktree_sessions`) - never every session across every
    /// worktree, per this revision's whole point (see `crate::root::mod`'s "One rail row per
    /// worktree" docs) - followed by one [`Self::render_file_tab`] per entry of
    /// [`Self::open_files`] in that `Vec`'s order (already worktree-scoped: `Self::
    /// select_worktree` clears it on every switch), then the `+` menu button
    /// ([`Self::render_tab_strip_plus`]) and right-aligned session-jump keycaps.
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

        let session_ids: Vec<SessionId> = self
            .current_worktree_sessions()
            .map(|session| session.id)
            .collect();
        for id in &session_ids {
            if let Some(session) = self.sessions.iter().find(|session| session.id == *id) {
                bar = bar.child(self.render_session_tab(session, cx));
            }
        }

        for path in &self.open_files {
            bar = bar.child(self.render_file_tab(path, cx));
        }

        bar = bar.child(self.render_tab_strip_plus(cx));

        let jump_keys = self.session_jump_keys();

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

    /// The real `secondary-1`..`secondary-8` session-jump keycap labels: one per session open in
    /// the *currently selected* worktree (`Self::current_worktree_sessions`), capped at 8 since
    /// those are the only ones actually bound (`crate::default_key_bindings`) - never a keycap
    /// advertising a shortcut that silently does nothing. Shared by [`Self::render_tab_strip`]'s
    /// own right-aligned cluster and the status bar's session hint (`root::status_bar::
    /// render_status_session_hint`), so the two can never independently drift on what's really
    /// bound.
    pub(super) fn session_jump_keys(&self) -> Vec<String> {
        let session_count = self.current_worktree_sessions().count().min(8);
        (1..=session_count).map(|n| n.to_string()).collect()
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
        // Real dirty-state indicator (Revision R8.5a): a small dot, shown only while this tab's
        // real `EditBuffer` genuinely has unsaved edits (`EditBuffer::is_dirty`) - `false` for a
        // tab with no buffer yet (still loading, or a truncated/read-only file - see
        // `AdeApp::edit_buffers`' own docs), never a fabricated placeholder.
        let is_dirty = self
            .edit_buffers
            .get(path)
            .is_some_and(|buffer| buffer.is_dirty());
        let drag_value = DraggedFileTab {
            path: path.to_path_buf(),
            label: file_name.clone(),
        };

        div()
            .id(format!("file-tab-{key}"))
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            // Real drag-to-reorder among file tabs - see `DraggedSessionTab`'s own docs for the
            // identical mechanism, mirrored here for `Self::open_files` instead of `Sessions`.
            .on_drag(drag_value, |dragged, _position, _window, cx| {
                cx.new(|_| dragged.clone())
            })
            .drag_over::<DraggedFileTab>(|tab, _dragged, _window, _cx| {
                tab.border_l(px(2.0)).border_color(theme::status::ASK)
            })
            .on_drop(cx.listener({
                let target = path.to_path_buf();
                move |this, dragged: &DraggedFileTab, _window, cx| {
                    this.reorder_open_file_before(&dragged.path, &target);
                    cx.notify();
                }
            }))
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
                    .when(is_dirty, |el| {
                        el.child(
                            div()
                                .id(format!("file-tab-dirty-{key}"))
                                .flex_none()
                                .w(px(6.0))
                                .h(px(6.0))
                                .rounded(theme::radius::CHIP)
                                .bg(theme::status::ASK),
                        )
                    })
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

    /// Moves `dragged` to sit immediately before `target` in [`Self::open_files`] - the file-tab
    /// strip's own drag-to-reorder backing, mirroring `crate::sessions::Sessions::move_before`'s
    /// identical shape for session tabs. A no-op if either path isn't currently open, or if
    /// they're the same path.
    pub(super) fn reorder_open_file_before(&mut self, dragged: &Path, target: &Path) {
        if dragged == target {
            return;
        }
        let Some(from) = self.open_files.iter().position(|path| path == dragged) else {
            return;
        };
        if !self.open_files.iter().any(|path| path == target) {
            return;
        }
        let path = self.open_files.remove(from);
        let to = self
            .open_files
            .iter()
            .position(|path| path == target)
            .unwrap_or(self.open_files.len());
        self.open_files.insert(to, path);
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
                theme::surface::SEGMENT_TRACK.into()
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
    /// Five rows: *New terminal* ([`Self::new_session`] with [`SessionKind::Shell`]), *New file*
    /// ([`Self::start_new_file`]), *New agent pane* ([`Self::new_agent_pane`]), *Open file…*
    /// ([`Self::open_palette`], scoped to [`palette::PaletteScope::Files`]), and *Next changed
    /// file* ([`Self::next_changed_file`]). *New terminal*, *New agent pane*, and *Next changed
    /// file* each dispatch the same method their own global keybinding does
    /// (`crate::default_key_bindings`) and show that binding's keycap; *New file* and *Open
    /// file…* have no global keybinding of their own (the latter's own docs cover a real
    /// Ctrl+P/readline conflict that ruled one out; *New file* simply has no design-specified
    /// shortcut) and so show no keycap. Every row's click handler also closes the menu.
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
                        render_dropdown_menu_row(
                            "\u{276f}",
                            theme::text::DIM.into(),
                            theme::surface::CHIP_NEUTRAL.into(),
                            "New terminal",
                            "in this worktree".to_string(),
                            keymap::resolve_combo("ctrl+shift+T", macos),
                            true,
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
                        render_dropdown_menu_row(
                            "+",
                            theme::text::DIM.into(),
                            theme::surface::CHIP_NEUTRAL.into(),
                            "New file",
                            "in this worktree".to_string(),
                            // No keycap: same reasoning as the "Open file…" row below - no
                            // global keybinding exists for this (see `Self::start_new_file`'s
                            // own docs).
                            Vec::new(),
                            true,
                        )
                        .on_click(cx.listener(
                            |this, _event: &ClickEvent, window, cx| {
                                this.plus_menu_open = false;
                                let cwd = this.active_session_cwd();
                                this.start_new_file(cwd, window, cx);
                            },
                        )),
                    )
                    .child(
                        render_dropdown_menu_row(
                            agent_initial,
                            agent_fg,
                            agent_bg,
                            "New agent pane",
                            agent_label.to_string(),
                            keymap::resolve_combo("mod+shift+N", macos),
                            true,
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
                        render_dropdown_menu_row(
                            "@",
                            theme::palette::COMMAND_CHIP.0.into(),
                            theme::palette::COMMAND_CHIP.1.into(),
                            "Open file\u{2026}",
                            "search this worktree".to_string(),
                            // No keycap: this row has no global keybinding (see the function
                            // docs above), and `render_keycap_row` renders nothing for `&[]`.
                            Vec::new(),
                            true,
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
                        render_dropdown_menu_row(
                            "]",
                            theme::text::DIM.into(),
                            theme::surface::CHIP_NEUTRAL.into(),
                            "Next changed file",
                            format!("{changed_count} changed"),
                            keymap::resolve_combo("]", macos),
                            true,
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
    /// (`Self::current_worktree_sessions`), via [`Self::select_session`]. No-op if fewer than
    /// `position` sessions are currently open in the selected worktree.
    pub(super) fn jump_to_session_at(
        &mut self,
        position: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = position
            .checked_sub(1)
            .and_then(|index| self.current_worktree_sessions().nth(index))
            .map(|session| session.id)
        else {
            return;
        };
        self.select_session(id, window, cx);
    }

    /// The Windows/Linux title bar's Session menu "Next session"/"Previous session" rows
    /// (`crate::root::title_bar::AdeApp::render_title_menu`) - `delta` is `1`/`-1`. Cycles
    /// through [`Self::current_worktree_sessions`] in the same order
    /// [`Self::jump_to_session_at`] indexes - **never** every session across every worktree
    /// (a real, live-reproduced bug found in this revision's own self-audit: an earlier version
    /// cycled `self.sessions` directly, so "Next Session" could jump to a *different* worktree's
    /// session, which [`Self::select_session`] then silently promotes into a full
    /// [`Self::select_worktree`] switch - discarding any unsaved `edit_buffers` content for the
    /// worktree just left via `reset_per_worktree_ui_state`. A menu row labeled "cycle tabs" must
    /// never have that side effect) - wrapping around both ends (mirroring
    /// [`Self::next_changed_file`]'s own cyclic-index convention for "next" over an existing
    /// ordered list), via the same real [`Self::select_session`] every tab-strip click and jump
    /// keycap already goes through - no separate "next session" subsystem, just a cyclic index
    /// over the existing per-worktree list. No-op with fewer than two sessions in the selected
    /// worktree (nothing to cycle to) or no active session at all (both real, reachable states -
    /// the latter only while every session has been closed).
    pub(super) fn select_relative_session(
        &mut self,
        delta: isize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<SessionId> = self.current_worktree_sessions().map(|s| s.id).collect();
        if ids.len() < 2 {
            return;
        }
        let Some(active_id) = self.sessions.active_id() else {
            return;
        };
        let Some(current_index) = ids.iter().position(|id| *id == active_id) else {
            return;
        };
        let len = ids.len() as isize;
        let next_index = (current_index as isize + delta).rem_euclid(len) as usize;
        self.select_session(ids[next_index], window, cx);
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
        let drag_value = DraggedSessionTab {
            id,
            label: label.clone(),
        };

        div()
            .id(("session-tab", id))
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            // Real drag-to-reorder (see `DraggedSessionTab`'s own docs): dragging this tab and
            // dropping it on another session tab in the same (per-worktree) strip moves it to
            // sit immediately before whichever tab it was dropped on
            // (`crate::sessions::Sessions::move_before`). No `can_drop` predicate needed - the
            // `on_drop::<DraggedSessionTab>` type parameter alone already rejects a drop of any
            // other dragged-value type (e.g. `DraggedFileTab`).
            .on_drag(drag_value, |dragged, _position, _window, cx| {
                cx.new(|_| dragged.clone())
            })
            .drag_over::<DraggedSessionTab>(|tab, _dragged, _window, _cx| {
                tab.border_l(px(2.0)).border_color(theme::status::ASK)
            })
            .on_drop(
                cx.listener(move |this, dragged: &DraggedSessionTab, _window, cx| {
                    this.sessions.move_before(dragged.id, id);
                    cx.notify();
                }),
            )
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

    /// Surface A/B's shared footer: git-level actions appropriate to the session's status - see
    /// `crate::work_surface::footer_actions`/[`Self::render_footer_action_button`] for which
    /// actions are implemented vs. disabled. No longer shows a `JERRY` wordmark (deliberate
    /// deviation from the design mockup, per direct user request - see this crate's `lib.rs`/
    /// `BUILD-LOG.md` for context, not a bug fix).
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
            .border_color(theme::border::INNER);

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
            // `Keep all`/`Discard worktree` (Revision R10) share one in-flight guard
            // (`Self::worktree_history_op_in_flight`) with `Undo`/`Redo` - see
            // `crate::root::worktree_history`'s own module docs for why one flag is enough
            // discipline here. Disabled, not just relabelled, while busy - mirrors
            // `Self::render_rail_footer`'s own `prune_in_flight` gating.
            let is_worktree_history_action = matches!(
                action.kind,
                work_surface::ActionKind::KeepAllChanges
                    | work_surface::ActionKind::DiscardWorktree
            );
            if is_worktree_history_action && self.worktree_history_op_in_flight.is_some() {
                enabled = false;
            }
            // Busy labels are keyed off the *specific* in-flight kind, not just "something is
            // running" - a real, live-reproduced bug an audit caught: keying this off the bare
            // in-flight flag alone made every visible `Discard worktree` button across every
            // session read "discarding…" while an unrelated `Undo` of a `Keep all` was running.
            let label = match action.kind {
                work_surface::ActionKind::DiscardWorktree
                    if self.worktree_history_op_in_flight
                        == Some(worktree_history::WorktreeHistoryOpKind::Discard) =>
                {
                    "discarding\u{2026}".to_string()
                }
                work_surface::ActionKind::DiscardWorktree
                    if self.discard_confirm_armed == Some(id) =>
                {
                    "confirm discard?".to_string()
                }
                work_surface::ActionKind::KeepAllChanges
                    if self.worktree_history_op_in_flight
                        == Some(worktree_history::WorktreeHistoryOpKind::Keep) =>
                {
                    "keeping\u{2026}".to_string()
                }
                _ => action.label.to_string(),
            };
            footer = footer.child(self.render_footer_action_button(
                id,
                cwd.clone(),
                action,
                label,
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
        label: String,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = work_surface::action_button_colors(action.style);
        let kind = action.kind;

        // Keyed off `kind`, not `label` - `label` now varies at render time (the confirm/busy
        // text swaps above), and an element's `id` should stay stable across that, matching
        // `Self::render_rail_footer`'s own static `"rail-prune"` id for its own label-swapping
        // button.
        let mut button = div()
            .id(format!("footer-action-{id}-{kind:?}"))
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
                theme::border::BUTTON_DISABLED.into()
            })
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        colors.fg
                    } else {
                        theme::text::GHOSTER.into()
                    })
                    .child(label),
            );

        if let Some(spec) = action.keycap {
            let (keycap_fg, keycap_border) = if enabled {
                (colors.keycap_fg, colors.keycap_border)
            } else {
                (
                    theme::text::GHOSTER.into(),
                    theme::border::BUTTON_DISABLED.into(),
                )
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
                        work_surface::ActionKind::KeepAllChanges => this.keep_all_changes(id, cx),
                        work_surface::ActionKind::DiscardWorktree => {
                            this.request_discard_worktree(id, window, cx)
                        }
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
        let surface = div()
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
                    .child(
                        "no sessions open in this worktree - start one with the tab strip's + menu",
                    ),
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
/// One row in either the tab strip's `+` menu ([`AdeApp::render_plus_menu`]) or the Windows/
/// Linux title bar's real File/Edit/View/Session/Help dropdowns
/// (`crate::root::title_bar::AdeApp::render_title_menu`) - shared here since both are the same
/// "labeled trigger opens a small popover of real actions" pattern, first built for the `+`
/// menu and reused as-is (not re-derived) for the title-bar menus.
///
/// `enabled` mirrors [`AdeApp::render_footer_action_button`]'s own enabled/disabled convention:
/// `false` dims the chip/label/sub text and drops `cursor_pointer`/hover, so a row a click
/// through it right now would have no real effect on (e.g. "Cut" with no active selection) reads
/// as visibly inert rather than looking exactly as actionable as a working row. Callers must
/// also skip attaching `.on_click` when `enabled` is `false` - this function only controls the
/// row's *look*, never whether it's wired up, so a disabled row a caller forgot to gate would
/// still silently do nothing on click, exactly the "looks actionable, does nothing" bug class
/// this project's discipline forbids.
pub(super) fn render_dropdown_menu_row(
    chip_glyph: &'static str,
    chip_fg: gpui::Rgba,
    chip_bg: gpui::Rgba,
    label: &'static str,
    sub: String,
    keys: Vec<String>,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    let (chip_fg, chip_bg, label_color): (gpui::Rgba, gpui::Rgba, gpui::Rgba) = if enabled {
        (chip_fg, chip_bg, theme::text::HEADING.into())
    } else {
        (
            theme::text::GHOSTER.into(),
            theme::surface::CHIP_NEUTRAL.into(),
            theme::text::GHOSTER.into(),
        )
    };
    let mut row = div()
        .id(format!("dropdown-menu-row-{label}"))
        .flex()
        .items_center()
        .gap(px(9.0))
        .h(theme::band::PLUS_MENU_ROW)
        .px(px(10.0));
    row = if enabled {
        row.cursor_pointer()
            .hover(|el| el.bg(theme::surface::PLUS_MENU_ROW_HOVER))
    } else {
        row.cursor_default()
    };
    row.child(
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
            .text_color(label_color)
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

/// The tab strip's drag-to-reorder value for a session tab (`Self::render_session_tab`'s
/// `on_drag`/`on_drop`) - real GPUI drag-and-drop (`on_drag`/`drag_over`/`can_drop`/`on_drop`,
/// verified against `vendor/zed/crates/gpui/src/elements/div.rs` and mirroring
/// `vendor/zed/crates/workspace/src/pane.rs`'s own `DraggedTab` for exactly this "reorder tabs
/// by dragging" pattern), not this project's earlier `on_drag`/`on_drag_move` use in
/// `crate::root::resize` (a resize-handle hack with no real drag *payload* at all). `Render`ing
/// the dragged value itself as its own small floating chip - the same choice Zed's own
/// `DraggedTab` makes - rather than a generic ghost, so what's being dragged stays legible.
#[derive(Clone)]
pub(super) struct DraggedSessionTab {
    pub(super) id: SessionId,
    pub(super) label: String,
}

impl Render for DraggedSessionTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::PALETTE)
            .border_1()
            .border_color(theme::border::POPOVER)
            .font(font(theme::font::SANS))
            .text_size(px(11.0))
            .text_color(theme::text::BODY)
            .child(self.label.clone())
    }
}

/// The file-tab strip's own drag-to-reorder value - see [`DraggedSessionTab`]'s own docs for the
/// shared mechanism; kept as a distinct type (not a shared enum) so a session tab can never be
/// accidentally dropped into a file-tab reorder or vice versa - GPUI's `on_drop::<T>` dispatches
/// purely on the dragged value's concrete type.
#[derive(Clone)]
pub(super) struct DraggedFileTab {
    pub(super) path: PathBuf,
    pub(super) label: String,
}

impl Render for DraggedFileTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(theme::radius::CHIP)
            .bg(theme::surface::PALETTE)
            .border_1()
            .border_color(theme::border::POPOVER)
            .font(font(theme::font::SANS))
            .text_size(px(11.0))
            .text_color(theme::text::BODY)
            .child(self.label.clone())
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

/// Regression coverage for this revision's core claim: each worktree gets one rail entry, and
/// its sessions become tabs scoped to whichever worktree's rail row is selected - the exact
/// behavior `crate::root::mod`'s "One rail row per worktree" module docs describe. Also covers
/// the real drag-to-reorder mechanism (`DraggedSessionTab`'s own docs).
#[cfg(test)]
mod tab_scoping_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::worktrees::WorktreeItem;
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

    fn seed_two_worktrees(app: &mut AdeApp, wt_a: PathBuf, wt_b: PathBuf) {
        app.worktrees = vec![worktree_item(wt_a, "wt-a"), worktree_item(wt_b, "wt-b")];
    }

    /// The exact bug `crate::rail::build_worktree_rows` fixes at the pure-logic level
    /// (`crate::rail::tests::build_worktree_rows_folds_every_session_in_a_worktree_not_just_the_first`),
    /// proven here end to end through the real `Sessions`/`AdeApp` plumbing: two sessions
    /// spawned into the same worktree must both show up as that worktree's tabs.
    #[gpui::test]
    fn multiple_sessions_in_one_worktree_all_show_as_tabs_under_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.sessions.spawn(
                SessionKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.sessions.spawn(
                SessionKind::Claude,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        let ids: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(
            ids.len(),
            2,
            "both sessions spawned into wt-a must show as tabs under it - not just the first \
             one found (the exact bug the old ProjectChild model had)"
        );
    }

    /// The real gap this revision's own self-audit found: switching to a worktree with *no*
    /// open session leaves `Sessions::focus_active` with nothing to focus (a genuine no-op), so
    /// a previously-focused session's pane - now unrendered once the tab strip's own
    /// per-worktree filter applies - would otherwise leave `Window::focus` dangling, breaking
    /// every global keybinding (including ⌘K itself) until the next click.
    /// `Self::select_worktree`'s own fallback (redirecting focus to `Self::filter_focus_handle`
    /// whenever the newly selected worktree has no session to focus) closes this.
    #[gpui::test]
    fn ctrl_k_still_works_after_switching_to_a_worktree_with_no_open_session(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_empty = tempfile::tempdir().expect("tempdir empty");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_empty.path().to_path_buf(), "empty")];
        });

        // Explicitly focus the initial shell session (the real, concrete "focus is on a live
        // terminal pane" starting state this bug needs), then switch to the session-less
        // worktree - the exact transition the fix targets.
        app.update_in(cx, |app, window, cx| {
            app.sessions.focus_active(window, cx);
            app.select_worktree(0, window, cx);
        });

        let key = if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        };
        cx.simulate_keystrokes(key);

        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after switching to a session-less worktree must still open \
             the palette - before the fix, focus was left dangling on the previous worktree's \
             now-unrendered terminal pane"
        );
    }

    /// Switching the rail selection to a different worktree must show that worktree's own tabs,
    /// not the previously selected worktree's - the centre-pane-follows-the-rail invariant, and
    /// the exact behavior `crate::root::mod`'s "One rail row per worktree" docs describe: never
    /// showing/pointing at the previously selected worktree's session.
    #[gpui::test]
    fn switching_worktree_selection_shows_that_worktrees_own_tabs(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let wt_b = tempfile::tempdir().expect("tempdir b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            seed_two_worktrees(app, wt_a.path().to_path_buf(), wt_b.path().to_path_buf());
        });

        let (id_a, id_b) = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let id_a = app.sessions.spawn(
                SessionKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.select_worktree(1, window, cx);
            let id_b = app.sessions.spawn(
                SessionKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (id_a, id_b)
        });

        // Still on wt-b (the last selection) - its tab strip must show only its own session.
        let current: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(current, vec![id_b]);
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(id_b)
        );

        // Switch back to wt-a - must show id_a, never id_b.
        app.update_in(cx, |app, window, cx| app.select_worktree(0, window, cx));
        let current: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(
            current,
            vec![id_a],
            "switching back to wt-a must show its own tab, not wt-b's"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(id_a),
            "the active session must follow the selected worktree"
        );
    }

    /// Closing the active tab in a worktree that still has another open tab must fall back to
    /// that sibling - the ordinary, non-degenerate case.
    #[gpui::test]
    fn closing_the_active_tab_falls_back_to_a_sibling_in_the_same_worktree(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_a.path().to_path_buf(), "wt-a")];
        });

        let (id1, id2) = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            let id1 = app.sessions.spawn(
                SessionKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let id2 = app.sessions.spawn(
                SessionKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (id1, id2)
        });
        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(id2)
        );

        app.update_in(cx, |app, window, cx| app.close_session(id2, window, cx));

        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            Some(id1),
            "closing the active tab must fall back to the remaining sibling in the same worktree"
        );
        let current: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(current, vec![id1]);
    }

    /// The degenerate case, and the real reason `Sessions::close`'s fallback had to become
    /// worktree-scoped rather than a same-`Vec` neighbor: closing the *last* tab in one worktree
    /// must never fall back to a different worktree's own still-open session, even though it
    /// might sit right next to it in the flat underlying storage.
    ///
    /// Also real, live-reproduced coverage for another instance of this project's own "focus
    /// left pointing at something unrendered" bug class (see `crate::root::focus`'s module doc):
    /// before the fix, `Self::close_session` left `Window::focus` dangling on the
    /// just-`shutdown()` `TerminalPane` in this exact case (`self.sessions.active = None`, and
    /// `Sessions::focus_active` is a real no-op with nothing active) - so a real ⌘K afterward,
    /// not just checking `active_id() == None`, is what actually proves focus isn't dangling,
    /// matching every other test for this bug class in this project.
    #[gpui::test]
    fn closing_the_last_tab_in_a_worktree_never_falls_back_to_a_different_worktrees_session(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_a = tempfile::tempdir().expect("tempdir a");
        let wt_b = tempfile::tempdir().expect("tempdir b");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        app.update(cx, |app, _cx| {
            seed_two_worktrees(app, wt_a.path().to_path_buf(), wt_b.path().to_path_buf());
        });

        let id_a = app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.sessions.spawn(
                SessionKind::Shell,
                wt_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            )
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
            app.sessions.spawn(
                SessionKind::Shell,
                wt_b.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
            app.close_session(id_a, window, cx);
        });

        assert_eq!(
            app.read_with(cx, |app, _| app.sessions.active_id()),
            None,
            "closing the only tab in wt-a must leave it with no active session, never silently \
             fall back to wt-b's own still-open session"
        );

        let key = if cfg!(target_os = "macos") {
            "cmd-k"
        } else {
            "ctrl-k"
        };
        cx.simulate_keystrokes(key);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after closing the last tab in a worktree must still open \
             the palette - before the fix, Window::focus was left dangling on the just-closed \
             session's now-unmounted pane, with nothing real for the next keystroke to reach"
        );
    }

    /// Real drag-to-reorder: dropping one session tab onto another must actually change their
    /// order in `Sessions`' own underlying storage (what `Self::render_tab_strip` iterates).
    #[gpui::test]
    fn drag_reordering_two_session_tabs_changes_their_order(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, id2, id3) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.sessions.active_id().expect("initial shell session");
            let id2 = app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            let id3 = app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (initial_id, id2, id3)
        });

        let before: Vec<SessionId> =
            app.read_with(cx, |app, _| app.sessions.iter().map(|s| s.id).collect());
        assert_eq!(before, vec![initial_id, id2, id3]);

        // The real drop handler's own logic: drag id3, drop it on `initial_id`'s tab.
        app.update(cx, |app, _cx| {
            app.sessions.move_before(id3, initial_id);
        });

        let after: Vec<SessionId> =
            app.read_with(cx, |app, _| app.sessions.iter().map(|s| s.id).collect());
        assert_eq!(
            after,
            vec![id3, initial_id, id2],
            "id3 must now sit immediately before initial_id, and id2 must be otherwise \
             untouched"
        );
    }

    /// `move_before` must never corrupt the tab order on a bad target - an unknown dragged id, an
    /// unknown drop target, or dropping a tab onto itself must all be real no-ops.
    #[gpui::test]
    fn drag_reorder_is_a_no_op_for_an_unknown_or_identical_id(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let initial_id = app.read_with(cx, |app, _| {
            app.sessions.active_id().expect("initial shell session")
        });

        app.update(cx, |app, _cx| {
            app.sessions.move_before(initial_id, initial_id);
            app.sessions.move_before(9999, initial_id);
            app.sessions.move_before(initial_id, 9999);
        });

        let after: Vec<SessionId> =
            app.read_with(cx, |app, _| app.sessions.iter().map(|s| s.id).collect());
        assert_eq!(
            after,
            vec![initial_id],
            "none of these malformed drops should have changed anything"
        );
    }
}
