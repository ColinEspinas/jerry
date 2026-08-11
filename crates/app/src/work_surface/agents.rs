//! Agent/tab bookkeeping: ties a spawned terminal process (a [`TerminalPane`]) to the
//! worktree it's running in, and tracks which agents are open and which one is active for
//! the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.
//!
//! ## Per-worktree tab scoping
//!
//! Every agent belongs to exactly one worktree (its [`Agent::cwd`]), and the centre pane's
//! tab strip (`crate::root::AdeApp::render_tab_strip`) only ever shows the agents belonging to
//! whichever worktree is currently selected - so [`Self::active`] doubles as "which agent is
//! shown in the centre pane" *and* must always name an agent in the currently selected
//! worktree (or be `None`, if that worktree has no open agent at all). [`Self::active_by_cwd`]
//! is the second half of that invariant: a per-worktree "last active tab" memory, so switching
//! back and forth between two worktrees each restores whichever tab you were last looking at in
//! each, rather than always landing on the first one. [`Self::activate_for_worktree`] is the one
//! place that invariant gets re-established - see `crate::root::AdeApp::select_worktree`'s own
//! docs for why it must also move real keyboard focus in the same step.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gpui::{App, AppContext as _, Context, Entity, Focusable as _, Subscription, Window};

use crate::root::AdeApp;
use crate::terminal::pane::{TerminalPane, TerminalPaneEvent, TerminalSpec};

/// What kind of process an agent runs. Purely descriptive - drives the tab label and which
/// "New ... Agent" button created it; `TerminalPane` itself has no branching for
/// "shell" vs. "agent CLI".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Shell,
    /// The `claude` CLI (Claude Code), spawned with no arguments in the chosen worktree.
    /// Resolved via `PATH`; if not installed, spawning fails and the pane shows
    /// `TerminalPane::spawn_error`.
    Claude,
    /// The `codex` CLI, spawned the same way as [`AgentKind::Claude`].
    Codex,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Shell => "Shell",
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
        }
    }

    /// `shell_override` is the user's configured shell
    /// (`crate::settings::store::TerminalSettings::shell_override`, GitHub issue #213) and only
    /// reaches [`AgentKind::Shell`]: an agent CLI spawns its own fixed binary directly, never
    /// through a shell, so which shell the user prefers genuinely cannot affect it.
    fn spec(self, cwd: PathBuf, shell_override: Option<&str>) -> TerminalSpec {
        // Reads through `agent_binary_name` rather than matching `Claude`/`Codex` again here,
        // so it stays the single source of truth for "what binary does this kind spawn".
        match self.agent_binary_name() {
            Some(binary) => TerminalSpec::command(binary, Vec::new(), cwd),
            None => TerminalSpec::shell(cwd, shell_override),
        }
    }

    /// The literal command name this kind spawns as - `None` for [`AgentKind::Shell`],
    /// which resolves the configured shell (or `$SHELL`) rather than a fixed binary name.
    ///
    /// Public so `crate::settings::state`'s Agents page can look up the same `$PATH` name this
    /// method hands `TerminalSpec::command` at spawn time, instead of a second hardcoded
    /// list that could drift from what's actually spawned.
    pub fn agent_binary_name(self) -> Option<&'static str> {
        match self {
            AgentKind::Shell => None,
            AgentKind::Claude => Some("claude"),
            AgentKind::Codex => Some("codex"),
        }
    }
}

/// A monotonically increasing agent id, stable across other agents closing - used
/// instead of a `Vec` index so a click handler capturing an id can't end up referring to the
/// wrong tab once some other tab closes and later indices shift down.
pub type AgentId = u64;

pub struct Agent {
    pub id: AgentId,
    pub kind: AgentKind,
    /// The worktree (or repo root) this agent's process was started in. Kept for the tab
    /// label/title; `TerminalPane` doesn't expose its own `cwd` back out.
    pub cwd: PathBuf,
    pub pane: Entity<TerminalPane>,
    /// When this agent was spawned - the rail agent row's real elapsed-time trailing text
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.3: "elapsed 9.5px mono
    /// right"). Set once, here, and never mutated afterwards - this app has no
    /// pause/resume-with-a-fresh-clock concept (see `crate::work_surface::state::pty_state_label`'s
    /// docs), so "elapsed" is simply wall-clock time since this real process was spawned.
    pub spawned_at: Instant,
    /// The same spawn moment as [`Self::spawned_at`], but as real wall-clock seconds since the
    /// Unix epoch - set from the identical call site, at the identical instant.
    ///
    /// Both exist because they answer genuinely different questions and neither can answer the
    /// other's. [`Instant`] is monotonic, which is exactly right for "how long has this been
    /// running" (the rail's elapsed text) and exactly wrong for identity: it has no meaning
    /// outside this process, and cannot be converted to a wall-clock time at all. This field is
    /// the durable half - part of the key a persisted review baseline is filed under
    /// (`crate::review::state::baseline_key`), since [`AgentId`] is a per-window counter that
    /// restarts at zero on every launch and so cannot key anything expected to outlive one.
    ///
    /// See `crate::review::baseline_state`'s own module docs for what that persistence can and
    /// cannot do today.
    pub spawned_at_unix: i64,
    /// Keeps [`Agents::spawn`]'s link-click-opens-a-file subscription (see
    /// [`TerminalPaneEvent`]) alive for this agent's lifetime - never read, only held.
    _link_subscription: Subscription,
}

/// Owns every open agent/tab and which one is active.
pub struct Agents {
    agents: Vec<Agent>,
    active: Option<AgentId>,
    /// The last-active agent id for each worktree that has (or has ever had) one - see the
    /// module docs' "Per-worktree tab scoping" section. An entry is removed once its worktree
    /// has no open agents left (see [`Self::close`]), rather than left pointing at a closed
    /// id, so [`Self::activate_for_worktree`] can tell "never visited"/"visited but now empty"
    /// apart from "has a real tab to restore" by nothing more than `HashMap::get`.
    active_by_cwd: HashMap<PathBuf, AgentId>,
    next_id: AgentId,
}

impl Agents {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            active: None,
            active_by_cwd: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Agent> {
        self.agents.iter()
    }

    /// Every currently open agent whose [`Agent::cwd`] is exactly `cwd` - the tab strip's
    /// real per-worktree filter (`crate::root::AdeApp::current_worktree_agents`), in the same
    /// stable creation order [`Self::iter`] itself returns. Takes `cwd` by value (rather than
    /// borrowing it) so the returned iterator doesn't need to borrow a caller-local `PathBuf` for
    /// as long as `self` - it owns its own copy instead, closed over by the filter closure.
    pub fn iter_for_cwd(&self, cwd: PathBuf) -> impl Iterator<Item = &Agent> {
        self.agents.iter().filter(move |agent| agent.cwd == cwd)
    }

    /// How many agents are currently open in `cwd` - the real signal behind GitHub issue #225's
    /// **single-agent gate**.
    ///
    /// The whole review surface (the Review tab, the footer's `Review diff` door, and the rail's
    /// review-ready status) is held back for any worktree with more than one open agent, because
    /// real per-agent attribution does not exist yet: with two agents sharing a worktree, an
    /// agent's review diff would honestly include changes the *other* one made, and this app
    /// would have no way to tell them apart. Presenting that as "this agent's review" would be a
    /// fabrication, so it shows nothing at all until the worktree is back down to one agent.
    ///
    /// Deliberately a **decision-time** check, not a capture-time one: a baseline is still
    /// captured for every agent regardless of how many share its worktree (it's cheap, and it
    /// means the surface simply starts working - correctly, against a real baseline taken at the
    /// right moment - the instant the others close). Only *display* is gated.
    ///
    /// Reads through [`Self::iter_for_cwd`] rather than open-coding the filter, so the tab
    /// strip's own per-worktree scoping and this gate can never disagree about which agents
    /// belong to a worktree.
    pub fn count_for_cwd(&self, cwd: &Path) -> usize {
        self.iter_for_cwd(cwd.to_path_buf()).count()
    }

    /// `true` if `id` names an agent that is the **only** one currently open in its own
    /// worktree: the review surface's real gate, in the one form every call site wants. `false`
    /// for an
    /// unknown id (e.g. a just-closed agent), so a stale click handler can never re-open a review
    /// surface for an agent that no longer exists.
    ///
    /// See [`Self::count_for_cwd`] for why this gate exists at all.
    pub fn is_sole_agent_in_worktree(&self, id: AgentId) -> bool {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return false;
        };
        self.count_for_cwd(&agent.cwd) == 1
    }

    pub fn active_id(&self) -> Option<AgentId> {
        self.active
    }

    pub fn active(&self) -> Option<&Agent> {
        let id = self.active?;
        self.agents.iter().find(|agent| agent.id == id)
    }

    /// Spawns a new agent of `kind` into `cwd` (the caller resolves this - see
    /// `crate::root::AdeApp::active_agent_cwd`), appends it as a new tab, and makes it
    /// active (both globally, [`Self::active`], and as `cwd`'s own remembered tab,
    /// [`Self::active_by_cwd`]). Returns the new agent's id.
    ///
    /// Deliberately does not move keyboard focus itself: only the caller
    /// (`crate::root::AdeApp`) knows whether a file tab is currently occupying the centre
    /// pane, and focusing an agent's pane while a file tab is showing points
    /// `Window::focus` at a node nothing in the rendered tree tracks. Every real call site
    /// guards this via `crate::root::AdeApp::focus_newly_spawned_agent`, whose own docs
    /// cover the bug this avoids.
    ///
    /// Subscribes to the new pane's [`TerminalPaneEvent`]s so a click on a detected
    /// path/`path:line` link in this agent's terminal output opens it as a file tab
    /// (`crate::root::AdeApp::open_terminal_link`). `terminal_font_size_px` and
    /// `shell_override` are both read fresh from live settings by every call site (the same
    /// threading pattern, for the same reason): a freshly spawned pane never starts out
    /// mismatched from what Settings shows. `shell_override` is GitHub issue #213's configured
    /// shell (`crate::settings::store::TerminalSettings::shell_override`) - `None` means "the
    /// real OS default", and it only affects [`AgentKind::Shell`] tabs (see
    /// [`AgentKind::spec`]).
    pub fn spawn(
        &mut self,
        kind: AgentKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        shell_override: Option<&str>,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        let id = self.next_id;
        self.next_id += 1;

        let spec = kind.spec(cwd.clone(), shell_override);
        let pane = cx.new(|cx| TerminalPane::new(spec, terminal_font_size_px, cx));
        let link_subscription =
            cx.subscribe_in(&pane, window, move |app, _pane, event, window, cx| {
                let TerminalPaneEvent::OpenPath { path, line } = event;
                app.open_terminal_link(path.clone(), *line, window, cx);
            });

        self.agents.push(Agent {
            id,
            kind,
            cwd: cwd.clone(),
            pane,
            spawned_at: Instant::now(),
            spawned_at_unix: unix_now(),
            _link_subscription: link_subscription,
        });
        self.active = Some(id);
        self.active_by_cwd.insert(cwd, id);
        self.sync_pane_cadence(cx);
        id
    }

    /// Re-derives every open pane's poll cadence from [`Self::active`]: exactly the active
    /// agent's pane is foreground (`TerminalPane::set_foreground`), every other pane is
    /// background. Called at the end of **every** mutator that can change which agent is
    /// active ([`Self::spawn`], [`Self::set_active`], [`Self::activate_for_worktree`],
    /// [`Self::close`]) - a full re-derivation over all panes rather than a delta update at
    /// each site, so no future mutator can leave a pane's cadence stale by forgetting the
    /// "demote the old one" half (this codebase's recurring stale-state bug class; a pane
    /// wrongly left background would lag visibly, one wrongly left foreground would quietly
    /// re-grow the multi-pane drain cost this flag exists to bound). Cheap enough for that:
    /// one flag write per open agent.
    fn sync_pane_cadence(&self, cx: &mut Context<AdeApp>) {
        for agent in &self.agents {
            let foreground = Some(agent.id) == self.active;
            agent
                .pane
                .update(cx, |pane, _| pane.set_foreground(foreground));
        }
    }

    /// Applies a Settings › Appearance "Terminal font size" edit to every currently open
    /// agent's pane, not just newly spawned ones. `TerminalPane::set_font_size` is a no-op
    /// for a pane already at that size, so calling this on every edit is cheap.
    pub fn set_terminal_font_size(&mut self, font_size_px: f32, cx: &mut Context<AdeApp>) {
        for agent in &self.agents {
            agent
                .pane
                .update(cx, |pane, cx| pane.set_font_size(font_size_px, cx));
        }
    }

    /// Makes `id` the globally active agent, and remembers it as its own worktree's active
    /// tab too - a no-op if `id` doesn't name a currently open agent. Takes `cx` (unlike a
    /// plain setter) because the active agent is what drives every pane's poll cadence -
    /// see [`Self::sync_pane_cadence`].
    pub fn set_active(&mut self, id: AgentId, cx: &mut Context<AdeApp>) {
        if let Some(agent) = self.agents.iter().find(|agent| agent.id == id) {
            self.active = Some(id);
            self.active_by_cwd.insert(agent.cwd.clone(), id);
            self.sync_pane_cadence(cx);
        }
    }

    /// Makes `cwd`'s own last-active tab (see [`Self::active_by_cwd`]) the globally active
    /// agent - or, if `cwd` has never had one recorded (a worktree just visited for the
    /// first time this window), its first open agent in creation order. `None` if `cwd`
    /// currently has no open agents at all.
    ///
    /// This is the real fix for the bug this revision exists to close: before it,
    /// [`Self::active`] was a single value entirely independent of which worktree was
    /// selected in the rail, so switching worktrees could leave the centre pane showing a
    /// completely different worktree's terminal. `crate::root::AdeApp::select_worktree` calls
    /// this on every switch - see that method's own docs for why it must also move real
    /// keyboard focus in the same step (the previously-active agent's pane may no longer be
    /// rendered at all once the tab strip's own per-worktree filter applies).
    pub fn activate_for_worktree(&mut self, cwd: &Path, cx: &mut Context<AdeApp>) {
        let remembered = self
            .active_by_cwd
            .get(cwd)
            .copied()
            .filter(|id| self.agents.iter().any(|agent| agent.id == *id));
        let id = remembered.or_else(|| {
            self.agents
                .iter()
                .find(|agent| agent.cwd == cwd)
                .map(|agent| agent.id)
        });
        self.active = id;
        if let Some(id) = id {
            self.active_by_cwd.insert(cwd.to_path_buf(), id);
        }
        self.sync_pane_cadence(cx);
    }

    /// Moves keyboard focus onto the currently active agent's terminal pane, if there is
    /// one - a no-op when [`Self::active`] is `None`. See [`Self::spawn`]'s docs for why
    /// callers, not this method, decide whether it's safe to call.
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(agent) = self.active() {
            window.focus(&agent.pane.focus_handle(cx), cx);
        }
    }

    /// Closes a tab: tears down its `PtySession` via `TerminalPane::shutdown` before dropping
    /// the `Entity<TerminalPane>`, so closing a tab never just hides it while its process
    /// leaks.
    ///
    /// The fallback for what becomes active next is scoped to the closed agent's *own
    /// worktree*, never a same-`Vec`-neighbor from a different one (pre-redesign, every
    /// agent shared one flat tab strip, so any neighbor was a reasonable fallback; now that
    /// the tab strip is per-worktree, falling back across worktrees would silently switch the
    /// centre pane to an unrelated worktree's terminal): the sibling that was immediately to
    /// the closed tab's right (in creation order, within the same `cwd`), falling back to its
    /// left, falling back to `None` if this was the worktree's last open tab - in which case
    /// its rail row simply reverts to the same "no active agent" state a worktree the user
    /// hasn't started anything in already shows (see `crate::rail::state::WorktreeRow`'s own docs);
    /// deliberately not auto-respawning a fresh shell, since an agent-less worktree is already
    /// a normal, existing state this app models everywhere else.
    ///
    /// Only moves focus (via [`Self::focus_active`]) when the closed agent was the *globally*
    /// active one - closing a background tab in a worktree that isn't currently selected must
    /// never steal focus - unless `skip_focus_move` says the centre pane isn't showing an
    /// agent's pane right now anyway (a file tab occupies it, or the whole workspace body is
    /// currently swapped out for Settings) - the caller, `crate::root::AdeApp::close_agent`,
    /// computes that from `AdeApp::open_change`/`AdeApp::settings_open` - see [`Self::spawn`]'s
    /// docs for why this can't be decided here.
    pub fn close(
        &mut self,
        id: AgentId,
        skip_focus_move: bool,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) {
        let Some(index) = self.agents.iter().position(|agent| agent.id == id) else {
            return;
        };
        let cwd = self.agents[index].cwd.clone();

        self.agents[index]
            .pane
            .update(cx, |pane, cx| pane.shutdown(cx));
        self.agents.remove(index);

        let sibling_indices: Vec<usize> = self
            .agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| agent.cwd == cwd)
            .map(|(sibling_index, _)| sibling_index)
            .collect();
        let new_cwd_active = sibling_indices
            .iter()
            .find(|&&sibling_index| sibling_index >= index)
            .or_else(|| sibling_indices.last())
            .map(|&sibling_index| self.agents[sibling_index].id);

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
        self.sync_pane_cadence(cx);
    }
}

/// Real wall-clock seconds since the Unix epoch, for [`Agent::spawned_at_unix`]. Mirrors
/// `crate::graph_view::render::unix_now` exactly, including its `unwrap_or(0)` for a system clock
/// somehow set before 1970 - a nonsense clock shouldn't stop an agent from spawning.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

impl Default for Agents {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `close`/`activate_for_worktree` both need a real `Agent` (a real `Entity<TerminalPane>`,
    /// only buildable via `Agents::spawn` inside a real GPUI window) to exercise meaningfully,
    /// so that coverage lives in `work_surface::render`'s GPUI-test module (`tab_scoping_tests`)
    /// instead, alongside the rest of this revision's worktree/tab scoping coverage (and the
    /// combined tab-order drag-to-reorder coverage - see `work_surface::state`'s own
    /// `reconcile_tab_order`/`move_tab_order` for the tab strip's real ordering layer, which
    /// tracks position on top of `Agents` rather than inside it). This module only holds the
    /// plain, GPUI-free checks that don't need a real pane at all.
    #[test]
    fn a_fresh_agents_collection_has_no_active_agent() {
        let agents = Agents::new();
        assert_eq!(agents.active_id(), None);
        assert!(agents.is_empty());
    }
}
