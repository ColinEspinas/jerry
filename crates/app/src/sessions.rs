//! Session/tab bookkeeping: ties a spawned terminal process (a [`TerminalPane`]) to the
//! worktree it's running in, and tracks which sessions are open and which one is active for
//! the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.
//!
//! ## Per-worktree tab scoping
//!
//! Every session belongs to exactly one worktree (its [`Session::cwd`]), and the centre pane's
//! tab strip (`crate::root::AdeApp::render_tab_strip`) only ever shows the sessions belonging to
//! whichever worktree is currently selected - so [`Self::active`] doubles as "which session is
//! shown in the centre pane" *and* must always name a session in the currently selected
//! worktree (or be `None`, if that worktree has no open session at all). [`Self::active_by_cwd`]
//! is the second half of that invariant: a per-worktree "last active tab" memory, so switching
//! back and forth between two worktrees each restores whichever tab you were last looking at in
//! each, rather than always landing on the first one. [`Self::activate_for_worktree`] is the one
//! place that invariant gets re-established - see `crate::root::AdeApp::select_worktree`'s own
//! docs for why it must also move real keyboard focus in the same step.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// The last-active session id for each worktree that has (or has ever had) one - see the
    /// module docs' "Per-worktree tab scoping" section. An entry is removed once its worktree
    /// has no open sessions left (see [`Self::close`]), rather than left pointing at a closed
    /// id, so [`Self::activate_for_worktree`] can tell "never visited"/"visited but now empty"
    /// apart from "has a real tab to restore" by nothing more than `HashMap::get`.
    active_by_cwd: HashMap<PathBuf, SessionId>,
    next_id: SessionId,
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active: None,
            active_by_cwd: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter()
    }

    /// Every currently open session whose [`Session::cwd`] is exactly `cwd` - the tab strip's
    /// real per-worktree filter (`crate::root::AdeApp::current_worktree_sessions`), in the same
    /// stable creation order [`Self::iter`] itself returns. Takes `cwd` by value (rather than
    /// borrowing it) so the returned iterator doesn't need to borrow a caller-local `PathBuf` for
    /// as long as `self` - it owns its own copy instead, closed over by the filter closure.
    pub fn iter_for_cwd(&self, cwd: PathBuf) -> impl Iterator<Item = &Session> {
        self.sessions
            .iter()
            .filter(move |session| session.cwd == cwd)
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
    /// active (both globally, [`Self::active`], and as `cwd`'s own remembered tab,
    /// [`Self::active_by_cwd`]). Returns the new session's id.
    ///
    /// Deliberately does not move keyboard focus itself: only the caller
    /// (`crate::root::AdeApp`) knows whether a file tab is currently occupying the centre
    /// pane, and focusing a session's pane while a file tab is showing points
    /// `Window::focus` at a node nothing in the rendered tree tracks. Every real call site
    /// guards this via `crate::root::AdeApp::focus_newly_spawned_session`, whose own docs
    /// cover the bug this avoids.
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
            cwd: cwd.clone(),
            pane,
            _link_subscription: link_subscription,
        });
        self.active = Some(id);
        self.active_by_cwd.insert(cwd, id);
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

    /// Makes `id` the globally active session, and remembers it as its own worktree's active
    /// tab too - a no-op if `id` doesn't name a currently open session.
    pub fn set_active(&mut self, id: SessionId) {
        if let Some(session) = self.sessions.iter().find(|session| session.id == id) {
            self.active = Some(id);
            self.active_by_cwd.insert(session.cwd.clone(), id);
        }
    }

    /// Makes `cwd`'s own last-active tab (see [`Self::active_by_cwd`]) the globally active
    /// session - or, if `cwd` has never had one recorded (a worktree just visited for the
    /// first time this window), its first open session in creation order. `None` if `cwd`
    /// currently has no open sessions at all.
    ///
    /// This is the real fix for the bug this revision exists to close: before it,
    /// [`Self::active`] was a single value entirely independent of which worktree was
    /// selected in the rail, so switching worktrees could leave the centre pane showing a
    /// completely different worktree's terminal. `crate::root::AdeApp::select_worktree` calls
    /// this on every switch - see that method's own docs for why it must also move real
    /// keyboard focus in the same step (the previously-active session's pane may no longer be
    /// rendered at all once the tab strip's own per-worktree filter applies).
    pub fn activate_for_worktree(&mut self, cwd: &Path) {
        let remembered = self
            .active_by_cwd
            .get(cwd)
            .copied()
            .filter(|id| self.sessions.iter().any(|session| session.id == *id));
        let id = remembered.or_else(|| {
            self.sessions
                .iter()
                .find(|session| session.cwd == cwd)
                .map(|session| session.id)
        });
        self.active = id;
        if let Some(id) = id {
            self.active_by_cwd.insert(cwd.to_path_buf(), id);
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
    /// leaks.
    ///
    /// The fallback for what becomes active next is scoped to the closed session's *own
    /// worktree*, never a same-`Vec`-neighbor from a different one (pre-redesign, every
    /// session shared one flat tab strip, so any neighbor was a reasonable fallback; now that
    /// the tab strip is per-worktree, falling back across worktrees would silently switch the
    /// centre pane to an unrelated worktree's terminal): the sibling that was immediately to
    /// the closed tab's right (in creation order, within the same `cwd`), falling back to its
    /// left, falling back to `None` if this was the worktree's last open tab - in which case
    /// its rail row simply reverts to the same "no active session" state a worktree the user
    /// hasn't started anything in already shows (see `crate::rail::WorktreeRow`'s own docs);
    /// deliberately not auto-respawning a fresh shell, since a session-less worktree is already
    /// a normal, existing state this app models everywhere else.
    ///
    /// Only moves focus (via [`Self::focus_active`]) when the closed session was the *globally*
    /// active one - closing a background tab in a worktree that isn't currently selected must
    /// never steal focus - unless `skip_focus_move` says the centre pane isn't showing a
    /// session's pane right now anyway (a file tab occupies it, or the whole workspace body is
    /// currently swapped out for Settings) - the caller, `crate::root::AdeApp::close_session`,
    /// computes that from `AdeApp::open_change`/`AdeApp::settings_open` - see [`Self::spawn`]'s
    /// docs for why this can't be decided here.
    pub fn close(
        &mut self,
        id: SessionId,
        skip_focus_move: bool,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) {
        let Some(index) = self.sessions.iter().position(|session| session.id == id) else {
            return;
        };
        let cwd = self.sessions[index].cwd.clone();

        self.sessions[index]
            .pane
            .update(cx, |pane, cx| pane.shutdown(cx));
        self.sessions.remove(index);

        let sibling_indices: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| session.cwd == cwd)
            .map(|(sibling_index, _)| sibling_index)
            .collect();
        let new_cwd_active = sibling_indices
            .iter()
            .find(|&&sibling_index| sibling_index >= index)
            .or_else(|| sibling_indices.last())
            .map(|&sibling_index| self.sessions[sibling_index].id);

        if self.active_by_cwd.get(&cwd) == Some(&id) {
            match new_cwd_active {
                Some(new_id) => {
                    self.active_by_cwd.insert(cwd.clone(), new_id);
                }
                None => {
                    self.active_by_cwd.remove(&cwd);
                }
            }
        }

        if self.active == Some(id) {
            self.active = new_cwd_active;
            if !skip_focus_move {
                self.focus_active(window, cx);
            }
        }
    }

    /// Moves session `dragged_id` to sit immediately before session `target_id` in this app's
    /// underlying tab order - the real backing for the tab strip's drag-to-reorder gesture
    /// (`crate::root::AdeApp::render_session_tab`'s `on_drop`). A no-op if either id is
    /// missing, or if they're the same id.
    ///
    /// Reorders within the single flat `Vec` shared by every worktree's sessions, not a
    /// per-worktree sub-list - safe because the tab strip only ever drags tabs that are
    /// already both visible in the same (per-worktree-filtered) strip, and inserting
    /// `dragged_id` immediately before `target_id`'s own position preserves their relative
    /// order in *any* filtered view that contains both, regardless of which other worktrees'
    /// sessions happen to be interleaved between them in the real storage order.
    pub fn move_before(&mut self, dragged_id: SessionId, target_id: SessionId) {
        if dragged_id == target_id {
            return;
        }
        let Some(from) = self
            .sessions
            .iter()
            .position(|session| session.id == dragged_id)
        else {
            return;
        };
        if !self.sessions.iter().any(|session| session.id == target_id) {
            return;
        }
        let session = self.sessions.remove(from);
        let to = self
            .sessions
            .iter()
            .position(|session| session.id == target_id)
            .unwrap_or(self.sessions.len());
        self.sessions.insert(to, session);
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `move_before`/`close`/`activate_for_worktree` all need a real `Session` (a real
    /// `Entity<TerminalPane>`, only buildable via `Sessions::spawn` inside a real GPUI window) to
    /// exercise meaningfully, so that coverage lives in `root::work_surface_render`'s GPUI-test
    /// module (`tab_scoping_tests`) instead, alongside the rest of this revision's worktree/tab
    /// scoping coverage. This module only holds the plain, GPUI-free checks that don't need a
    /// real pane at all.
    #[test]
    fn a_fresh_sessions_collection_has_no_active_session() {
        let sessions = Sessions::new();
        assert_eq!(sessions.active_id(), None);
        assert!(sessions.is_empty());
    }
}
