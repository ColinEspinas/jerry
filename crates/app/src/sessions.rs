//! Session/tab bookkeeping: ties a real spawned terminal process (a [`TerminalPane`]) to
//! the worktree it's running in, and tracks which sessions are open and which one is active
//! for the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.

use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, Focusable as _, Window};

use crate::root::AdeApp;
use crate::terminal_pane::{TerminalPane, TerminalSpec};

/// What kind of process a session runs. Purely descriptive - it drives the tab label and
/// which "New ... Session" button created it - not behavioral: `TerminalPane` spawns
/// whatever `TerminalSpec` [`SessionKind::spec`] hands it and has no branching of its own
/// for "shell" vs. "agent CLI" (see its module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Shell,
    /// The real `claude` CLI (Claude Code), spawned as `claude` with no arguments - an
    /// interactive agent session in the chosen worktree. Resolved via `PATH` the same way a
    /// shell resolves any bare command name (see `TerminalSpec::command`'s docs); if
    /// `claude` isn't installed, spawning fails and the pane shows a real, non-panicking
    /// spawn error (`TerminalPane::spawn_error`), not simulated output.
    Claude,
    /// The real `codex` CLI, spawned the same way as [`SessionKind::Claude`]. Not installed
    /// on this dev machine (verified: `which codex` finds nothing) - this is also this
    /// step's real, honest exercise of the spawn-failure path: clicking "New Codex Session"
    /// here genuinely fails to spawn (a real `PATH` lookup miss via `pty-core`'s
    /// `portable_pty::CommandBuilder`, not a simulated failure) and surfaces through the
    /// same `TerminalPane::spawn_error` UI as any other real spawn error would.
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
        // Reads through `agent_binary_name` (rather than matching `Claude`/`Codex` directly a
        // second time) so it stays the one real source of truth for "what literal command name
        // does this kind spawn" - see that method's docs. Falls back to a real shell rather than
        // `.unwrap_or_default()`-ing to `""` if some future `SessionKind` variant were added
        // without also being taught to `agent_binary_name`: spawning `""` would silently fail in
        // a confusing way, while a shell session is at least a real, working process instead of
        // a silent misspawn.
        match self.agent_binary_name() {
            Some(binary) => TerminalSpec::command(binary, Vec::new(), cwd),
            None => TerminalSpec::shell(cwd),
        }
    }

    /// The literal command name this kind's real process is spawned as (see [`Self::spec`],
    /// which calls this directly) - `None` for [`SessionKind::Shell`], which resolves `$SHELL`
    /// rather than a single fixed binary name, so "the binary name" has no single real answer
    /// for it.
    ///
    /// Exposed (not just an internal implementation detail of `spec`) so `crate::settings`'s
    /// real Settings › Agents page - which needs to know *what name a real `$PATH` search
    /// should look for* to show a genuine ready/not-found status per agent - reads the exact
    /// same literal this method already hands `TerminalSpec::command` at spawn time, rather
    /// than maintaining a second, separately written `"claude"`/`"codex"` list that could
    /// silently drift from what actually gets spawned.
    pub fn agent_binary_name(self) -> Option<&'static str> {
        match self {
            SessionKind::Shell => None,
            SessionKind::Claude => Some("claude"),
            SessionKind::Codex => Some("codex"),
        }
    }
}

/// A monotonically increasing session id, stable across other sessions closing - used
/// instead of a `Vec` index so a click handler capturing an id (rather than an index)
/// can't end up referring to the wrong tab after some other tab closes and every later
/// index shifts down.
pub type SessionId = u64;

pub struct Session {
    pub id: SessionId,
    pub kind: SessionKind,
    /// The worktree (or repo root - see `Sessions::spawn`'s docs) this session's process
    /// was started in. Kept for the tab label/title; `TerminalPane` itself doesn't expose
    /// its `cwd` back out.
    pub cwd: PathBuf,
    pub pane: Entity<TerminalPane>,
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

    /// Spawns a new session of `kind` into `cwd` (the caller resolves this: the selected
    /// worktree's real path, or the repo root if none is selected - see
    /// `crate::root::AdeApp::active_session_cwd`), appends it as a new tab, and makes it the
    /// active tab. Returns the new session's id.
    ///
    /// ## Real keyboard focus is the caller's job, not this method's
    ///
    /// This used to also move `Window::focus` onto the new session's own pane unconditionally,
    /// right here - correct as long as the centre pane only ever showed a session, but a real,
    /// live-reproduced bug once Revision R4a's unified tab strip let a *file* tab be the thing
    /// showing instead: `crate::root::AdeApp::render_center_pane` never mounts any
    /// `TerminalPane` while a file tab (`AdeApp::open_change`) is active - so spawning a second
    /// session while a file tab was showing still pointed `Window::focus` at a pane
    /// `render_center_pane` never renders that frame. GPUI's own focus resolution falls back to
    /// the dispatch tree's synthetic root node when that happens - a node that sits *above*
    /// every one of `crate::root::AdeApp::render`'s own `on_action` handlers, so **every** bound
    /// keyboard shortcut would silently stop working (reproduced live: open a file tab, press
    /// `ctrl-shift-t`, then `ctrl-k` did nothing) until the next click re-established real
    /// focus.
    ///
    /// Only the caller - `crate::root::AdeApp` - knows whether a file tab is currently the thing
    /// occupying the centre pane, so the decision of *whether* to move focus onto the freshly
    /// spawned session now lives there too: every real call site
    /// (`AdeApp::new_session`/`open_companion_terminal`/`respawn_session`/`new_agent_pane`, plus
    /// the initial shell `AdeApp::new_with_settings` spawns) calls [`Self::focus_active`]
    /// itself, guarded by `AdeApp::open_change.is_none()`, right after this returns.
    pub fn spawn(
        &mut self,
        kind: SessionKind,
        cwd: PathBuf,
        cx: &mut Context<AdeApp>,
    ) -> SessionId {
        let id = self.next_id;
        self.next_id += 1;

        let spec = kind.spec(cwd.clone());
        let pane = cx.new(|cx| TerminalPane::new(spec, cx));

        self.sessions.push(Session {
            id,
            kind,
            cwd,
            pane,
        });
        self.active = Some(id);
        id
    }

    pub fn set_active(&mut self, id: SessionId) {
        if self.sessions.iter().any(|session| session.id == id) {
            self.active = Some(id);
        }
    }

    /// Moves real keyboard focus onto the currently active session's own terminal pane, if
    /// there is one - a real no-op (nothing focused) when [`Self::active`] is `None`. Shared by
    /// every real caller that needs to (re)point `Window::focus` at "whichever session is active
    /// right now": [`Self::spawn`]'s own real call sites (see that method's docs for why the
    /// guard itself has to live in the caller) and [`Self::close`] (see that method's docs).
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(session) = self.active() {
            window.focus(&session.pane.focus_handle(cx), cx);
        }
    }

    /// Closes a tab: deterministically tears down its real `PtySession` via
    /// `TerminalPane::shutdown` (see that method's docs) *before* dropping the
    /// `Entity<TerminalPane>`, so closing a tab never just hides it while its process leaks.
    /// If the closed tab was active, activates the tab that slides into its old index
    /// (i.e. its right neighbor), falling back to its left neighbor, falling back to no
    /// active tab if it was the last one open - and, if a new tab did become active,
    /// moves real keyboard focus onto its pane via [`Self::focus_active`], unless
    /// `file_tab_active` says a file tab is what's really occupying the centre pane right now
    /// (the same real condition, and the same real dangling-`Window::focus` bug, [`Self::spawn`]'s
    /// own docs describe - the caller, `crate::root::AdeApp::close_session`, computes it from
    /// `AdeApp::open_change.is_some()`, the one thing here that can't know it itself).
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
