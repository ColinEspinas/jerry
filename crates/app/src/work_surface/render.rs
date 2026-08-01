use super::*;
use crate::root::widgets::{
    render_action_keycap_row, render_env_chip, render_hint_pair, render_keycap_row, text_tooltip,
    KeycapSize,
};
use gpui::DragMoveEvent;

/// Defines one `JumpToSessionN` action handler forwarding a literal position to
/// [`AdeApp::jump_to_session_at`]. Each `actions!`-generated struct is a distinct action type
/// with no positional data, so GPUI needs one `on_action` handler per keystroke regardless; this
/// macro just keeps the eight near-identical bodies from drifting from each other.
macro_rules! session_jump_action_handler {
    ($fn_name:ident, $action:ty, $position:expr) => {
        pub(crate) fn $fn_name(
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
    pub(crate) fn new_session(
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
    pub(crate) fn focus_newly_spawned_session(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.open_change.is_none() && !self.settings_open {
            self.sessions.focus_active(window, cx);
        }
    }

    pub(crate) fn handle_new_session_action(
        &mut self,
        _action: &NewSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, window, cx);
    }

    /// `Ctrl+W` (GitHub issue #26) - closes whichever tab the centre pane is genuinely showing
    /// right now: a file tab (via [`crate::code_surface::tabs::AdeApp::request_close_file_tab`],
    /// the real unsaved-changes-confirming entry point every other close gesture already uses) if
    /// [`AdeApp::open_change`] is `Some`, else the globally active session tab (via
    /// [`Self::close_session`], which already tears down its real child process cleanly - SIGHUP,
    /// a bounded grace period, then `SIGKILL` - see `pty_core::PtySession::shutdown`'s own docs;
    /// nothing here reimplements that). A genuine no-op, never a window close, whenever there is
    /// no real tab to close: Settings is showing over the workspace body (`AdeApp::settings_open`,
    /// meaning nothing tab-like is on screen to act on), or the active worktree already has no
    /// open tab at all (the real "last tab closed" end state this app leaves alone rather than
    /// spawning a replacement or closing the window - this app registers no window-close
    /// keybinding at all, on any platform, so there is no native "Ctrl+W closes the window"
    /// default here to accidentally fall back to in the first place).
    ///
    /// Scoped to `Some("!terminal")` in `crate::default_key_bindings` (not global) - plain
    /// `Ctrl+W` is `crate::terminal::pane::keystroke_to_bytes`'s own real `unix-word-rerase`
    /// control byte (`0x17`) a focused shell needs for its own word-backspace; see that list's
    /// own docs (and its `secondary-z`/`"]"` entries) for this project's established precedent
    /// for exactly this "app-level shortcut steals terminal input" conflict class (`secondary-p`
    /// is the one deliberate exception to that precedent, not a further example of it - see that
    /// binding's own docs) - a live terminal pane keeps `Ctrl+W`, and closing a *terminal* tab
    /// this way stays reachable through the tab strip's own `×`/middle-click instead.
    pub(crate) fn handle_close_focused_tab_action(
        &mut self,
        _action: &CloseFocusedTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings_open {
            return;
        }
        if let Some(path) = self.open_change.clone() {
            self.request_close_file_tab(path, window, cx);
            return;
        }
        if let Some(id) = self.sessions.active_id() {
            self.close_session(id, window, cx);
            cx.notify();
        }
    }

    /// Activates session `id`'s tab and, if it maps to a currently-listed worktree, also selects
    /// that worktree, keeping the file tree/diff sidebar in sync with the session just clicked
    /// (the sidebar is still driven by [`Self::selected`] - a `focused_session`-driven Zone 2/3
    /// hasn't been rebuilt yet). If a file tab was active, this deactivates it
    /// (`Self::open_change = None`, without closing it - it stays in [`Self::open_files`]) and
    /// restores focus onto the session's pane via [`restore_focus`].
    pub(crate) fn select_session(
        &mut self,
        id: SessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sessions.set_active(id, cx);
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        if self.open_change.is_some() {
            self.open_change = None;
            self.refresh_open_diff_file_cache();
            self.hover = None;
            // See `crate::code_surface::tabs::AdeApp::open_and_focus_file`'s identical
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
    pub(crate) fn session_status(&self, session: &Session, cx: &App) -> Status {
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
    pub(crate) fn archive_session(
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
    /// keyboard focus falls back onto [`Self::rail_focus_handle`] - the same fallback
    /// [`Self::select_worktree`] uses for the identical "nothing left to focus" case - so
    /// `Window::focus` never stays pointed at the just-`shutdown()`, no-longer-rendered pane. The
    /// rail's *root*, not its filter field (which this used to target): see
    /// [`Self::rail_focus_handle`]'s own docs for the real keystroke-swallowing bug that was.
    pub(crate) fn close_session(
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
            window.focus(&self.rail_focus_handle, cx);
        }
    }

    /// The surface footer's `Interrupt ⌃C` action - sends `Ctrl-C` to the session's pty via
    /// `TerminalPane::interrupt`.
    pub(in crate::work_surface) fn interrupt_session(
        &mut self,
        id: SessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let pane = session.pane.clone();
        pane.update(cx, |pane, cx| pane.interrupt(cx));
    }

    /// The surface footer's `Retry ⌘R` (failed sessions) / `Resume ⌘⏎` (idle sessions) action.
    /// This app has no saved-session resumability to resume *from* (see
    /// `crate::work_surface::state::pty_state_label`'s docs), so the honest equivalent is: close this
    /// tab, then spawn a fresh session of the same kind into the same worktree - not literally
    /// "resume where it left off" (`crate::work_surface::state::ActionKind::Respawn`'s docs name this
    /// trade-off).
    pub(in crate::work_surface) fn respawn_session(
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
    /// process (`crate::work_surface::sessions`'s module docs), so "open terminal" just means "get me a shell
    /// in this worktree", the same capability as the rail's "+ New Shell" button.
    pub(in crate::work_surface) fn open_companion_terminal(
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

    /// The active worktree's real combined tab order (GitHub issue #16) - every session and file
    /// tab currently open in it, interleaved exactly as [`Self::render_tab_strip`] draws them,
    /// instead of always "every session, then every file". Reconciled fresh from
    /// [`Self::tab_order`]'s stored order plus whatever's *actually* open right now
    /// (`work_surface::state::reconcile_tab_order`'s own docs on why this is safe to call on
    /// every render rather than caching a mutated copy) - [`Self::tab_order`] itself only records
    /// a user's real drag-chosen order, never which tabs exist; that's still `Sessions`/
    /// [`Self::open_files`]'s job.
    pub(crate) fn combined_tab_order(&self) -> Vec<work_surface::TabRef> {
        let cwd = self.active_session_cwd();
        let stored = self.tab_order.get(&cwd).map(Vec::as_slice).unwrap_or(&[]);
        let session_ids: Vec<SessionId> = self
            .sessions
            .iter_for_cwd(cwd.clone())
            .map(|session| session.id)
            .collect();
        work_surface::reconcile_tab_order(stored, &session_ids, &self.open_files)
    }

    /// The unified tab strip's real drag-to-reorder entry point (GitHub issue #16) - moves
    /// `dragged` to sit immediately before (or, if `insert_after`, immediately after) `target` in
    /// the active worktree's own combined tab order, regardless of whether either is a session or
    /// a file tab (`work_surface::state::move_tab_order`'s own docs on why this is one function,
    /// not a per-kind pair). Persists the result into [`Self::tab_order`], keyed by the active
    /// worktree's cwd, so it survives the next render's [`Self::combined_tab_order`]
    /// reconciliation and, for session tabs, a later worktree switch away and back. Never
    /// restarts a pty or reloads a file buffer - `Sessions`/[`Self::open_files`] themselves are
    /// untouched; only this ordering layer changes.
    pub(in crate::work_surface) fn reorder_tab(
        &mut self,
        dragged: work_surface::TabRef,
        target: work_surface::TabRef,
        insert_after: bool,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.active_session_cwd();
        let mut order = self.combined_tab_order();
        work_surface::move_tab_order(&mut order, &dragged, &target, insert_after);
        self.tab_order.insert(cwd, order);
        cx.notify();
    }

    /// One tab's own `on_drag_move::<DraggedTab>` handler (both [`Self::render_session_tab`] and
    /// [`Self::render_file_tab`] register this, each closing over its own `hovered` [`work_surface::
    /// TabRef`]) - real per-tab mouse tracking during a drag, not a container-level listener:
    /// `on_drag_move`'s own doc comment and `crate::root::scrollbar`'s module docs both confirm
    /// GPUI dispatches a matching `on_drag_move::<T>` to *every* mounted element of that type on
    /// every drag-move tick, each receiving its own `event.bounds` - so every tab checks whether
    /// the live cursor (`event.event.position`) actually falls inside its own bounds before
    /// claiming the insertion caret; whichever tab's bounds the cursor is really over is the one
    /// that wins, on the very next tick after the cursor crosses into it. Splits `hovered`'s own
    /// width in half (`Bounds::center`) to decide "before" (`insert_after = false`, left half) vs.
    /// "after" (`insert_after = true`, right half) - the exact cursor-position precision GitHub
    /// issue #16 asks for, replacing the old whole-tab `border_l` highlight that couldn't tell
    /// the two apart. A no-op while the dragged tab is hovering over *itself* - dropping a tab on
    /// its own slot is always a no-op ([`work_surface::state::move_tab_order`]'s own docs), so no
    /// caret should invite it either.
    pub(in crate::work_surface) fn update_tab_drag_insertion(
        &mut self,
        hovered: &work_surface::TabRef,
        event: &DragMoveEvent<DraggedTab>,
        cx: &mut Context<Self>,
    ) {
        if event.drag(cx).tab_ref() == *hovered {
            return;
        }
        if !event.bounds.contains(&event.event.position) {
            return;
        }
        let insert_after = event.event.position.x >= event.bounds.center().x;
        if self.tab_drag_insertion.as_ref() != Some(&(hovered.clone(), insert_after)) {
            self.tab_drag_insertion = Some((hovered.clone(), insert_after));
            cx.notify();
        }
    }

    /// One tab's own `on_drop::<DraggedTab>` handler (both [`Self::render_session_tab`] and
    /// [`Self::render_file_tab`] register this) - reads which half of `target`'s own tab the
    /// cursor last landed on from [`Self::tab_drag_insertion`] (defaulting to "before" if the
    /// drop lands on a tab [`Self::update_tab_drag_insertion`] never actually recorded for - a
    /// drop can still fire on a tab the cursor technically never entered, e.g. a very fast
    /// release), then delegates to [`Self::reorder_tab`], and clears the now-stale caret state.
    pub(in crate::work_surface) fn drop_dragged_tab(
        &mut self,
        dragged: work_surface::TabRef,
        target: work_surface::TabRef,
        cx: &mut Context<Self>,
    ) {
        let insert_after = self
            .tab_drag_insertion
            .as_ref()
            .is_some_and(|(hovered, after)| *hovered == target && *after);
        self.reorder_tab(dragged, target, insert_after, cx);
        self.tab_drag_insertion = None;
    }

    /// Every session open in the *currently selected* worktree (`Self::active_session_cwd`), in
    /// the same order [`Self::combined_tab_order`] renders them - never Sessions' own raw
    /// creation order once a real drag has interleaved them differently, and never every session
    /// across every worktree, per this revision's whole point (see `crate::root::mod`'s "One
    /// rail row per worktree" docs). The real per-worktree tab-strip order both
    /// [`Self::render_tab_strip`] and [`Self::session_jump_keys`]/[`Self::jump_to_session_at`]
    /// share, so the tabs shown and the tabs a jump keycap can reach can never disagree.
    pub(crate) fn current_worktree_sessions(&self) -> impl Iterator<Item = &Session> {
        let order = self.combined_tab_order();
        order.into_iter().filter_map(move |tab_ref| match tab_ref {
            work_surface::TabRef::Session(id) => {
                self.sessions.iter().find(|session| session.id == id)
            }
            work_surface::TabRef::File(_) => None,
        })
    }

    /// The tab strip: one tab per entry of [`Self::combined_tab_order`], in that exact order -
    /// [`Self::render_session_tab`] for a `TabRef::Session`, [`Self::render_file_tab`] for a
    /// `TabRef::File` - so a session tab and a file tab can sit side by side in either order
    /// (GitHub issue #16), rather than always "every session, then every file" - followed by the
    /// `+` menu button ([`Self::render_tab_strip_plus`]) and right-aligned session-jump keycaps.
    pub(in crate::work_surface) fn render_tab_strip(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut bar = div()
            .id("tab-strip")
            .flex()
            .flex_none()
            .items_stretch()
            .h(theme::band::TAB_STRIP)
            .bg(theme::surface::TITLE_BAR)
            .border_b_1()
            .border_color(theme::border::ZONE);

        for tab_ref in self.combined_tab_order() {
            match tab_ref {
                work_surface::TabRef::Session(id) => {
                    if let Some(session) = self.sessions.iter().find(|session| session.id == id) {
                        bar = bar.child(self.render_session_tab(session, cx));
                    }
                }
                work_surface::TabRef::File(path) => {
                    bar = bar.child(self.render_file_tab(&path, cx));
                }
            }
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
    /// own right-aligned cluster and the status bar's session hint (`status_bar::render::
    /// render_status_session_hint`), so the two can never independently drift on what's really
    /// bound.
    pub(crate) fn session_jump_keys(&self) -> Vec<String> {
        let session_count = self.current_worktree_sessions().count().min(8);
        (1..=session_count).map(|n| n.to_string()).collect()
    }

    /// A file tab: language chip (`file_tree::lang_chip_for_name`, dimmed via
    /// `work_surface::file_tab_chip_colors` when inactive), file name, and a close hit box.
    /// Clicking the body activates the tab ([`Self::activate_file_tab`]); clicking `×`, middle-
    /// clicking anywhere on the tab, or the global `Ctrl+W` (GitHub issue #26) all close it via
    /// [`crate::code_surface::tabs::AdeApp::request_close_file_tab`] (never [`Self::close_file_tab`]
    /// directly - see that method's own docs for the real unsaved-changes confirmation this keeps
    /// every close gesture honest about), stopping propagation so a close never also activates
    /// (the same pattern [`render_session_tab`]'s close button uses). Shares active/inactive bg/
    /// underline/label colours with session tabs (`work_surface::tab_colors`).
    pub(in crate::work_surface) fn render_file_tab(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_active = self.open_change.as_deref() == Some(path);
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let lang = file_tree::lang_chip_for_name(&file_name);
        let chip_colors = work_surface::file_tab_chip_colors(lang, is_active);
        let colors = work_surface::tab_colors(is_active);
        // `Self::close_tab_confirm_armed` (GitHub issue #26): a real, visible cue - not just an
        // internal flag - that this specific dirty tab is one more close gesture away from really
        // closing without saving, matching `Self::prune_status`'s own "the confirmation state is
        // always on screen, never silent" precedent for this app's other two-gesture confirmations.
        let is_close_armed = self.close_tab_confirm_armed.as_deref() == Some(path);
        let close_color = if is_close_armed {
            theme::button::DANGER_FG
        } else if is_active {
            theme::text::DIMMER
        } else {
            theme::text::DISABLED
        };
        let activate_path = path.to_path_buf();
        let close_path = activate_path.clone();
        let middle_click_path = activate_path.clone();
        let key = path.display().to_string();
        // Real dirty-state indicator (Revision R8.5a): a small dot, shown only while this tab's
        // real `EditBuffer` genuinely has unsaved edits (`EditBuffer::is_dirty`) - `false` for a
        // tab with no buffer yet (still loading, or a truncated/read-only file - see
        // `AdeApp::edit_buffers`' own docs), never a fabricated placeholder.
        let is_dirty = self
            .edit_buffers
            .get(path)
            .is_some_and(|buffer| buffer.is_dirty());
        let tab_ref = work_surface::TabRef::File(path.to_path_buf());
        let drag_value = DraggedTab::File {
            path: path.to_path_buf(),
            label: file_name.clone(),
        };
        let insertion_caret = match &self.tab_drag_insertion {
            Some((target, insert_after)) if *target == tab_ref => Some(*insert_after),
            _ => None,
        };

        div()
            .id(format!("file-tab-{key}"))
            .relative()
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            // Middle-click closes any file tab outright (GitHub issue #26), same real
            // `request_close_file_tab` entry point as `×`/`Ctrl+W` - so a dirty tab still gets the
            // real unsaved-changes confirmation rather than a middle-click silently bypassing it.
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.request_close_file_tab(middle_click_path.clone(), window, cx);
                }),
            )
            // Real drag-to-reorder, unified across session and file tabs (GitHub issue #16) -
            // see `DraggedTab`'s own docs for the shared mechanism.
            .on_drag(drag_value, |dragged, _position, _window, cx| {
                cx.new(|_| dragged.clone())
            })
            .on_drag_move(cx.listener({
                let tab_ref = tab_ref.clone();
                move |this, event: &DragMoveEvent<DraggedTab>, _window, cx| {
                    this.update_tab_drag_insertion(&tab_ref, event, cx);
                }
            }))
            .on_drop(cx.listener({
                let target = tab_ref.clone();
                move |this, dragged: &DraggedTab, _window, cx| {
                    this.drop_dragged_tab(dragged.tab_ref(), target.clone(), cx);
                }
            }))
            .when_some(insertion_caret, |el, insert_after| {
                el.child(render_tab_insertion_caret(insert_after))
            })
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
                            // Real, visible confirmation cue (GitHub issue #26) - see
                            // `close_color`'s own docs above for why `is_close_armed` never leaves
                            // this a silent internal-only flag.
                            .when(is_close_armed, |el| {
                                el.tooltip(text_tooltip(
                                    "Unsaved changes - click × again to close without saving",
                                ))
                            })
                            .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                this.request_close_file_tab(close_path.clone(), window, cx);
                            })),
                    ),
            )
            .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline))
    }

    /// The tab strip's `+` menu button - toggles [`Self::plus_menu_open`] (unconditionally
    /// spawning a shell is the rail's separate `+` -
    /// [`crate::rail::render::render_new_session_button`]). A `gpui::canvas` child captures
    /// this button's painted bounds into [`Self::plus_button_bounds`] every render, which
    /// [`Self::render_plus_menu`] positions the popover off of. Opening the menu also refreshes
    /// [`Self::load_agent_rows`], so the "New agent pane" row's sub-label
    /// ([`Self::resolved_new_agent_kind`]) reflects a reasonably fresh `$PATH` search rather than
    /// a possibly-empty cached snapshot.
    pub(in crate::work_surface) fn render_tab_strip_plus(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
    /// file…* have no global keybinding of their own (the latter's own real `mod+P` spec is now
    /// claimed by `TogglePalette` instead - see `crate::default_key_bindings`'s own docs for that
    /// tradeoff; *New file* simply has no design-specified shortcut) and so show no keycap. Every
    /// row's click handler also closes the menu.
    pub(crate) fn render_plus_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
    pub(crate) fn resolved_new_agent_kind(&self) -> SessionKind {
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
    pub(in crate::work_surface) fn new_agent_pane(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn next_changed_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(crate) fn jump_to_session_at(
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
    /// (`crate::title_bar::menu::AdeApp::render_title_menu`) - `delta` is `1`/`-1`. Cycles
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
    pub(crate) fn select_relative_session(
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
    pub(crate) fn handle_new_terminal_action(
        &mut self,
        _action: &NewTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, window, cx);
    }

    /// [`NewAgentPane`]'s `secondary-shift-n` action handler - see [`Self::new_agent_pane`].
    pub(crate) fn handle_new_agent_pane_action(
        &mut self,
        _action: &NewAgentPane,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_agent_pane(cx);
    }

    /// [`NextChangedFile`]'s `]` action handler - see [`Self::next_changed_file`].
    pub(crate) fn handle_next_changed_file_action(
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
    pub(in crate::work_surface) fn render_session_tab(
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
        let tab_ref = work_surface::TabRef::Session(id);
        let drag_value = DraggedTab::Session {
            id,
            label: label.clone(),
        };
        let insertion_caret = match &self.tab_drag_insertion {
            Some((target, insert_after)) if *target == tab_ref => Some(*insert_after),
            _ => None,
        };

        div()
            .id(("session-tab", id))
            .relative()
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            // Middle-click closes any session/terminal tab too (GitHub issue #26) - the same
            // `Self::close_session` real teardown (`TerminalPane::shutdown`'s SIGHUP/grace/
            // SIGKILL - see that method's own docs) every other close path already uses.
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(move |this, _event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    this.close_session(id, window, cx);
                    cx.notify();
                }),
            )
            // Real drag-to-reorder, unified across session and file tabs (GitHub issue #16) -
            // see `DraggedTab`'s own docs for the shared mechanism. No `can_drop` predicate
            // needed - the `on_drop::<DraggedTab>` type parameter alone already rejects a drop of
            // any other dragged-value type.
            .on_drag(drag_value, |dragged, _position, _window, cx| {
                cx.new(|_| dragged.clone())
            })
            .on_drag_move(cx.listener({
                let tab_ref = tab_ref.clone();
                move |this, event: &DragMoveEvent<DraggedTab>, _window, cx| {
                    this.update_tab_drag_insertion(&tab_ref, event, cx);
                }
            }))
            .on_drop(cx.listener({
                let target = tab_ref.clone();
                move |this, dragged: &DraggedTab, _window, cx| {
                    this.drop_dragged_tab(dragged.tab_ref(), target.clone(), cx);
                }
            }))
            .when_some(insertion_caret, |el, insert_after| {
                el.child(render_tab_insertion_caret(insert_after))
            })
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
    pub(in crate::work_surface) fn render_session_context_bar(
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
    pub(in crate::work_surface) fn render_merge_button(
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
    pub(in crate::work_surface) fn render_archive_button(
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
    /// globally would risk the same "app-level shortcut steals terminal input" class of bug
    /// `crate::default_key_bindings`'s own `"]"`/`secondary-z` entries deliberately scope away
    /// from - unlike `secondary-p`, which the project ultimately accepted eating a focused
    /// terminal's own Ctrl+P as a deliberate, discussed tradeoff (see that binding's own docs),
    /// there's no equivalent case made here for doing the same to `kill-line`. Zed's own
    /// keymaps confirm this isn't overcaution: `terminal::Clear` is bound to `ctrl-shift-l` on
    /// Linux/Windows and reserved for `cmd-k` on macOS alone, where a platform-modified keystroke
    /// never reaches the pty in the first place (`crate::terminal::pane::keystroke_to_bytes`).
    pub(in crate::work_surface) fn render_pty_header(
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
    pub(in crate::work_surface) fn render_pty_info_footer(
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
    /// `crate::work_surface::state::footer_actions`/[`Self::render_footer_action_button`] for which
    /// actions are implemented vs. disabled. No longer shows a `JERRY` wordmark (deliberate
    /// deviation from the design mockup, per direct user request - see this crate's `lib.rs`/
    /// `BUILD-LOG.md` for context, not a bug fix).
    pub(in crate::work_surface) fn render_pty_footer(
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
            // `crate::work_surface::state::ActionKind::Respawn`'s docs) - disable it in that case
            // rather than letting a click spawn a redundant duplicate session.
            if action.kind == work_surface::ActionKind::Respawn
                && status_value == Status::Idle
                && is_running
            {
                enabled = false;
            }
            // `Keep all`/`Discard worktree` (Revision R10) share one in-flight guard
            // (`Self::worktree_history_op_in_flight`) with `Undo`/`Redo` - see
            // `crate::worktree_history::flow`'s own module docs for why one flag is enough
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
    pub(in crate::work_surface) fn render_footer_action_button(
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
    pub(crate) fn render_center_pane(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
/// (`crate::title_bar::menu::AdeApp::render_title_menu`) - shared here since both are the same
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
pub(crate) fn render_dropdown_menu_row(
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
pub(in crate::work_surface) fn render_tab_chip(
    kind: SessionKind,
    active: bool,
) -> gpui::AnyElement {
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

/// The unified tab strip's drag-to-reorder value (GitHub issue #16) - both
/// [`AdeApp::render_session_tab`] and [`AdeApp::render_file_tab`]'s `on_drag`/`on_drag_move`/
/// `on_drop` share this one type (rather than the two separate, kind-locked types this revision
/// replaces) precisely so a session tab can be dropped onto a file tab and vice versa: GPUI's
/// `on_drop::<T>`/`on_drag_move::<T>` dispatch purely on the dragged value's concrete type
/// (verified against `vendor/zed/crates/gpui/src/elements/div.rs`, and
/// `crate::root::scrollbar`'s own module docs on that exact dispatch rule), so two distinct types
/// could never cross-target each other's drop handlers - real GPUI drag-and-drop
/// (`on_drag`/`on_drag_move`/`on_drop`), not this project's earlier `on_drag`/`on_drag_move` use
/// in `crate::root::resize` (a resize-handle hack with no real drag *payload* at all), and
/// mirroring `vendor/zed/crates/workspace/src/pane.rs`'s own `DraggedTab` for the "reorder tabs
/// by dragging" pattern itself. `Render`ing the dragged value as its own small floating chip -
/// the same choice Zed's own `DraggedTab` makes - keeps what's being dragged legible.
#[derive(Clone)]
pub(in crate::work_surface) enum DraggedTab {
    Session { id: SessionId, label: String },
    File { path: PathBuf, label: String },
}

impl DraggedTab {
    fn label(&self) -> &str {
        match self {
            DraggedTab::Session { label, .. } => label,
            DraggedTab::File { label, .. } => label,
        }
    }

    /// This dragged value's own identity as a [`work_surface::TabRef`] - what
    /// [`AdeApp::reorder_tab`] actually moves, regardless of which concrete kind was dragged.
    pub(in crate::work_surface) fn tab_ref(&self) -> work_surface::TabRef {
        match self {
            DraggedTab::Session { id, .. } => work_surface::TabRef::Session(*id),
            DraggedTab::File { path, .. } => work_surface::TabRef::File(path.clone()),
        }
    }
}

impl Render for DraggedTab {
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
            .child(self.label().to_string())
    }
}

/// A precise insertion-point marker (GitHub issue #16's "better visual feedback" ask) - a thin
/// vertical bar at the exact boundary a dropped tab would land on: a hovered tab's own left edge
/// if `insert_after` is `false` (the dragged tab would land immediately before it), its right
/// edge if `insert_after` is `true` (immediately after) - replacing the old whole-tab `border_l`
/// highlight, which never distinguished "before" from "after" the hovered tab, with an
/// unambiguous "it lands exactly here" caret instead.
fn render_tab_insertion_caret(insert_after: bool) -> impl IntoElement {
    div()
        .absolute()
        .top(px(0.0))
        .bottom(px(0.0))
        .w(px(2.0))
        .bg(theme::status::ASK)
        .when(insert_after, |el| el.right(px(0.0)))
        .when(!insert_after, |el| el.left(px(0.0)))
}

/// The session context bar's status pill: a coloured dot plus label in the status colour.
pub(in crate::work_surface) fn render_status_pill(status: Status) -> impl IntoElement {
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

    fn seed_two_worktrees(app: &mut AdeApp, wt_a: PathBuf, wt_b: PathBuf) {
        app.worktrees = vec![worktree_item(wt_a, "wt-a"), worktree_item(wt_b, "wt-b")];
    }

    /// The exact bug `crate::rail::state::build_worktree_rows` fixes at the pure-logic level
    /// (`crate::rail::state::tests::build_worktree_rows_folds_every_session_in_a_worktree_not_just_the_first`),
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
    /// every global keybinding (including ⌘P itself) until the next click.
    /// `Self::select_worktree`'s own fallback (redirecting focus to `Self::filter_focus_handle`
    /// whenever the newly selected worktree has no session to focus) closes this.
    #[gpui::test]
    fn ctrl_p_still_works_after_switching_to_a_worktree_with_no_open_session(
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
            "cmd-p"
        } else {
            "ctrl-p"
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
    /// `Sessions::focus_active` is a real no-op with nothing active) - so a real ⌘P afterward,
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
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(key);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after closing the last tab in a worktree must still open \
             the palette - before the fix, Window::focus was left dangling on the just-closed \
             session's now-unmounted pane, with nothing real for the next keystroke to reach"
        );
    }

    /// Real drag-to-reorder, unified across session and file tabs (GitHub issue #16): dropping
    /// one session tab onto another must actually change the *combined* tab order
    /// (`Self::current_worktree_sessions`, which now reads `Self::combined_tab_order` rather than
    /// `Sessions`' own raw creation order - see that method's own docs on why).
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

        let before: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(before, vec![initial_id, id2, id3]);

        // The real drop handler's own logic: drag id3, drop it before `initial_id`'s tab.
        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::Session(id3),
                work_surface::TabRef::Session(initial_id),
                false,
                cx,
            );
        });

        let after: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(
            after,
            vec![id3, initial_id, id2],
            "id3 must now sit immediately before initial_id, and id2 must be otherwise \
             untouched"
        );
    }

    /// `Self::reorder_tab`/`work_surface::state::move_tab_order` must never corrupt the tab order
    /// on a bad target - an unknown dragged id, an unknown drop target, or dropping a tab onto
    /// itself must all be real no-ops.
    #[gpui::test]
    fn drag_reorder_is_a_no_op_for_an_unknown_or_identical_id(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let initial_id = app.read_with(cx, |app, _| {
            app.sessions.active_id().expect("initial shell session")
        });

        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::Session(initial_id),
                work_surface::TabRef::Session(initial_id),
                false,
                cx,
            );
            app.reorder_tab(
                work_surface::TabRef::Session(9999),
                work_surface::TabRef::Session(initial_id),
                false,
                cx,
            );
            app.reorder_tab(
                work_surface::TabRef::Session(initial_id),
                work_surface::TabRef::Session(9999),
                false,
                cx,
            );
        });

        let after: Vec<SessionId> = app.read_with(cx, |app, _| {
            app.current_worktree_sessions().map(|s| s.id).collect()
        });
        assert_eq!(
            after,
            vec![initial_id],
            "none of these malformed drops should have changed anything"
        );
    }

    /// Exactly the globally active session's pane may poll at the frame-accurate foreground
    /// cadence (`TerminalPane::is_foreground`); every other open pane must be demoted to the
    /// background cadence - through every real mutator of "which session is active": spawn,
    /// tab click (`select_session`), closing the active tab, and switching to a session-less
    /// worktree. A pane wrongly left foreground silently re-grows the measured multi-pane
    /// foreground-drain regression this flag exists to bound (see
    /// `crate::terminal::pane::BACKGROUND_POLL_INTERVAL`'s docs); one wrongly left background would lag
    /// the very pane the user is watching.
    #[gpui::test]
    fn only_the_active_sessions_pane_polls_at_the_foreground_cadence(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let wt_empty = tempfile::tempdir().expect("tempdir empty");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let foreground_ids =
            |app: &gpui::Entity<AdeApp>, cx: &mut TestAppContext| -> Vec<SessionId> {
                app.read_with(cx, |app, cx| {
                    app.sessions
                        .iter()
                        .filter(|s| s.pane.read(cx).is_foreground())
                        .map(|s| s.id)
                        .collect()
                })
            };

        let (first_id, second_id) = app.update_in(cx, |app, window, cx| {
            let first_id = app.sessions.active_id().expect("initial shell session");
            let second_id = app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (first_id, second_id)
        });

        // Spawning made the new session active - it alone must be foreground.
        assert_eq!(
            foreground_ids(&app, cx),
            vec![second_id],
            "after spawn, only the newly active session's pane may be foreground"
        );

        // A real tab click: the clicked session's pane is promoted, the old one demoted.
        app.update_in(cx, |app, window, cx| {
            app.select_session(first_id, window, cx);
        });
        assert_eq!(
            foreground_ids(&app, cx),
            vec![first_id],
            "selecting a tab must promote exactly that pane and demote the previous one"
        );

        // Closing the active tab promotes the surviving sibling.
        app.update_in(cx, |app, window, cx| {
            app.sessions.close(first_id, false, window, cx);
        });
        assert_eq!(
            foreground_ids(&app, cx),
            vec![second_id],
            "closing the active tab must hand the foreground cadence to the promoted sibling"
        );

        // Switching to a worktree with no sessions: nothing is active, nothing is watchable -
        // every pane must be background.
        app.update(cx, |app, _cx| {
            app.worktrees = vec![worktree_item(wt_empty.path().to_path_buf(), "empty")];
        });
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
        });
        assert_eq!(
            foreground_ids(&app, cx),
            Vec::<SessionId>::new(),
            "with no active session, no pane may keep the foreground cadence"
        );
    }

    /// The real cross-kind capability GitHub issue #16 exists to unlock: a file tab dragged so it
    /// lands between two session tabs must actually interleave them in the combined tab order,
    /// not just reorder within its own kind - the exact case the old, kind-locked
    /// `DraggedSessionTab`/`DraggedFileTab` types could never produce (GPUI's `on_drop::<T>`
    /// dispatches purely on the dragged value's concrete type, so a `DraggedFileTab` could never
    /// be dropped onto a session tab's `on_drop::<DraggedSessionTab>` handler or vice versa).
    #[gpui::test]
    fn dragging_a_file_tab_between_two_session_tabs_interleaves_them(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let file_path = repo.path().join("a.txt");
        std::fs::write(&file_path, "hello\n").expect("write a.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.sessions.active_id().expect("initial shell session");
            let second_id = app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            app.open_file_view(file_path.clone(), window, cx);
            (initial_id, second_id)
        });

        let before = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            before,
            vec![
                work_surface::TabRef::Session(initial_id),
                work_surface::TabRef::Session(second_id),
                work_surface::TabRef::File(PathBuf::from("a.txt")),
            ],
            "with no drag yet, sessions come first (creation order), then files - the old \
             two-block layout"
        );

        // The real cross-kind drop: drag the file tab so it lands between the two session tabs.
        app.update(cx, |app, cx| {
            app.reorder_tab(
                work_surface::TabRef::File(PathBuf::from("a.txt")),
                work_surface::TabRef::Session(second_id),
                false,
                cx,
            );
        });

        let after = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            after,
            vec![
                work_surface::TabRef::Session(initial_id),
                work_surface::TabRef::File(PathBuf::from("a.txt")),
                work_surface::TabRef::Session(second_id),
            ],
            "the file tab must now sit between the two session tabs - the real cross-group \
             interleaving this revision exists to unlock"
        );
    }

    /// `Self::drop_dragged_tab` must actually honor whichever half of the target tab the cursor
    /// was last recorded over (`Self::tab_drag_insertion`, set by
    /// `Self::update_tab_drag_insertion`'s own real per-tab `on_drag_move` tracking) - "after"
    /// must land the dragged tab on the far side of the target from a plain "before" drop - and
    /// must clear that state once handled, so a later drop on a different tab can't inherit it.
    #[gpui::test]
    fn drop_dragged_tab_honors_the_recorded_insertion_side_then_clears_it(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (initial_id, second_id) = app.update_in(cx, |app, window, cx| {
            let initial_id = app.sessions.active_id().expect("initial shell session");
            let second_id = app.sessions.spawn(
                SessionKind::Shell,
                repo.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
            (initial_id, second_id)
        });

        // Simulates what a real `on_drag_move` tick over the right half of `second_id`'s tab
        // would already have recorded, just before the drop.
        app.update(cx, |app, _cx| {
            app.tab_drag_insertion = Some((work_surface::TabRef::Session(second_id), true));
        });

        app.update(cx, |app, cx| {
            app.drop_dragged_tab(
                work_surface::TabRef::Session(initial_id),
                work_surface::TabRef::Session(second_id),
                cx,
            );
        });

        let order = app.read_with(cx, |app, _| app.combined_tab_order());
        assert_eq!(
            order,
            vec![
                work_surface::TabRef::Session(second_id),
                work_surface::TabRef::Session(initial_id),
            ],
            "insert_after == true must land the dragged tab immediately after the target, not \
             before"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.tab_drag_insertion.clone()),
            None,
            "a handled drop must clear the now-stale insertion-caret state"
        );
    }
}
