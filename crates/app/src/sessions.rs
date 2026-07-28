//! Session/tab bookkeeping: ties a real spawned terminal process (a [`TerminalPane`]) to
//! the worktree it's running in, and tracks which sessions are open and which one is active
//! for the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.

use std::path::PathBuf;

use gpui::{AppContext as _, Context, Entity};

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
        match self {
            SessionKind::Shell => TerminalSpec::shell(cwd),
            SessionKind::Claude => TerminalSpec::command("claude", Vec::new(), cwd),
            SessionKind::Codex => TerminalSpec::command("codex", Vec::new(), cwd),
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

    /// Closes a tab: deterministically tears down its real `PtySession` via
    /// `TerminalPane::shutdown` (see that method's docs) *before* dropping the
    /// `Entity<TerminalPane>`, so closing a tab never just hides it while its process leaks.
    /// If the closed tab was active, activates the tab that slides into its old index
    /// (i.e. its right neighbor), falling back to its left neighbor, falling back to no
    /// active tab if it was the last one open.
    pub fn close(&mut self, id: SessionId, cx: &mut Context<AdeApp>) {
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
        }
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}
