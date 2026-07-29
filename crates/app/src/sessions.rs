//! Session/tab bookkeeping: ties a spawned terminal process (a [`TerminalPane`]) to the
//! worktree it's running in, and tracks which sessions are open and which one is active for
//! the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.

use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, Focusable as _, Subscription, Window};

use crate::root::AdeApp;
use crate::terminal_pane::{TerminalPane, TerminalPaneEvent, TerminalSpec};

/// What kind of process a session runs. Purely descriptive - drives the tab label and which
/// "New ... Session" button created it; `TerminalPane` itself has no branching for
/// "shell" vs. "agent CLI".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Shell,
    /// The `claude` CLI (Claude Code), spawned with no arguments in the chosen worktree.
    /// Resolved via `PATH`; if not installed, spawning fails and the pane shows
    /// `TerminalPane::spawn_error`.
    Claude,
    /// The `codex` CLI, spawned the same way as [`SessionKind::Claude`].
    Codex,
}

impl SessionKind {
    pub fn label(self) -> &'static str {
        match self {
            SessionKind::Shell => "Shell",
            SessionKind::Claude => "Claude",
            SessionKind::Codex => "Codex",
        }
    }

    fn spec(self, cwd: PathBuf) -> TerminalSpec {
        // Reads through `agent_binary_name` rather than matching `Claude`/`Codex` again here,
        // so it stays the single source of truth for "what binary does this kind spawn".
        match self.agent_binary_name() {
            Some(binary) => TerminalSpec::command(binary, Vec::new(), cwd),
            None => TerminalSpec::shell(cwd),
        }
    }

    /// The literal command name this kind spawns as - `None` for [`SessionKind::Shell`],
    /// which resolves `$SHELL` rather than a fixed binary name.
    ///
    /// Public so `crate::settings`'s Agents page can look up the same `$PATH` name this
    /// method hands `TerminalSpec::command` at spawn time, instead of a second hardcoded
    /// list that could drift from what's actually spawned.
    pub fn agent_binary_name(self) -> Option<&'static str> {
        match self {
            SessionKind::Shell => None,
            SessionKind::Claude => Some("claude"),
            SessionKind::Codex => Some("codex"),
        }
    }
}

/// A monotonically increasing session id, stable across other sessions closing - used
/// instead of a `Vec` index so a click handler capturing an id can't end up referring to the
/// wrong tab once some other tab closes and later indices shift down.
pub type SessionId = u64;

pub struct Session {
    pub id: SessionId,
    pub kind: SessionKind,
    /// The worktree (or repo root) this session's process was started in. Kept for the tab
    /// label/title; `TerminalPane` doesn't expose its own `cwd` back out.
    pub cwd: PathBuf,
    pub pane: Entity<TerminalPane>,
    /// Keeps [`Sessions::spawn`]'s link-click-opens-a-file subscription (see
    /// [`TerminalPaneEvent`]) alive for this session's lifetime - never read, only held.
    _link_subscription: Subscription,
}

/// Owns every open session/tab and which one is active.
pub struct Sessions {
    sessions: Vec<Session>,
    active: Option<SessionId>,
    next_id: SessionId,
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active: None,
            next_id: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter()
    }

    pub fn active_id(&self) -> Option<SessionId> {
        self.active
    }

    pub fn active(&self) -> Option<&Session> {
        let id = self.active?;
        self.sessions.iter().find(|session| session.id == id)
    }

    /// Spawns a new session of `kind` into `cwd` (the caller resolves this - see
    /// `crate::root::AdeApp::active_session_cwd`), appends it as a new tab, and makes it
    /// active. Returns the new session's id.
    ///
    /// Deliberately does not move keyboard focus itself: only the caller
    /// (`crate::root::AdeApp`) knows whether a file tab is currently occupying the centre
    /// pane, and focusing a session's pane while a file tab is showing points
    /// `Window::focus` at a node nothing in the rendered tree tracks - GPUI falls back to a
    /// dispatch-tree root with no `on_action` handlers, silently breaking every keyboard
    /// shortcut until the next click. Every real call site guards this via
    /// `crate::root::AdeApp::focus_newly_spawned_session`, whose own docs cover the bug this
    /// avoids.
    ///
    /// Subscribes to the new pane's [`TerminalPaneEvent`]s so a click on a detected
    /// path/`path:line` link in this session's terminal output opens it as a file tab
    /// (`crate::root::AdeApp::open_terminal_link`). `terminal_font_size_px` is read fresh
    /// from live settings by every call site, so a freshly spawned pane never starts out
    /// mismatched from what Settings › Appearance shows.
    pub fn spawn(
        &mut self,
        kind: SessionKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> SessionId {
        let id = self.next_id;
        self.next_id += 1;

        let spec = kind.spec(cwd.clone());
        let pane = cx.new(|cx| TerminalPane::new(spec, terminal_font_size_px, cx));
        let link_subscription =
            cx.subscribe_in(&pane, window, move |app, _pane, event, window, cx| {
                let TerminalPaneEvent::OpenPath { path, line } = event;
                app.open_terminal_link(path.clone(), *line, window, cx);
            });

        self.sessions.push(Session {
            id,
            kind,
            cwd,
            pane,
            _link_subscription: link_subscription,
        });
        self.active = Some(id);
        id
    }

    /// Applies a Settings › Appearance "Terminal font size" edit to every currently open
    /// session's pane, not just newly spawned ones. `TerminalPane::set_font_size` is a no-op
    /// for a pane already at that size, so calling this on every edit is cheap.
    pub fn set_terminal_font_size(&mut self, font_size_px: f32, cx: &mut Context<AdeApp>) {
        for session in &self.sessions {
            session
                .pane
                .update(cx, |pane, cx| pane.set_font_size(font_size_px, cx));
        }
    }

    pub fn set_active(&mut self, id: SessionId) {
        if self.sessions.iter().any(|session| session.id == id) {
            self.active = Some(id);
        }
    }

    /// Moves keyboard focus onto the currently active session's terminal pane, if there is
    /// one - a no-op when [`Self::active`] is `None`. See [`Self::spawn`]'s docs for why
    /// callers, not this method, decide whether it's safe to call.
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(session) = self.active() {
            window.focus(&session.pane.focus_handle(cx), cx);
        }
    }

    /// Closes a tab: tears down its `PtySession` via `TerminalPane::shutdown` before dropping
    /// the `Entity<TerminalPane>`, so closing a tab never just hides it while its process
    /// leaks. If the closed tab was active, activates its right neighbor, falling back to its
    /// left, falling back to none - then moves focus via [`Self::focus_active`] unless
    /// `file_tab_active` says a file tab occupies the centre pane right now (the caller,
    /// `crate::root::AdeApp::close_session`, computes that from `AdeApp::open_change` - see
    /// [`Self::spawn`]'s docs for why this can't be decided here).
    pub fn close(
        &mut self,
        id: SessionId,
        file_tab_active: bool,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) {
        let Some(index) = self.sessions.iter().position(|session| session.id == id) else {
            return;
        };

        self.sessions[index]
            .pane
            .update(cx, |pane, cx| pane.shutdown(cx));
        self.sessions.remove(index);

        if self.active == Some(id) {
            self.active = if index < self.sessions.len() {
                Some(self.sessions[index].id)
            } else if index > 0 {
                Some(self.sessions[index - 1].id)
            } else {
                None
            };
            if !file_tab_active {
                self.focus_active(window, cx);
            }
        }
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}
