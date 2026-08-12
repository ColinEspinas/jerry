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

/// Which agent CLI a real agent runs. Never a bare shell - see [`ProcessKind`] for the type that
/// also covers a plain interactive terminal.
///
/// This type existing *without* a `Shell` variant is the point: anything that only makes sense
/// for a real agent (the Settings › Agents "installed on `$PATH`?" card, the `$PATH` name to
/// search for, which tint/initial an agent badge wears) can take an `AgentKind` and be
/// structurally impossible to call with a shell, rather than having to remember a runtime
/// "is this Shell?" check it could silently forget - which is exactly the bug class this shape
/// replaced (`crate::rail::status::derive_status`'s [`crate::rail::status::Status::Review`] arm
/// genuinely forgot it, back when one flat enum held all three cases as equal peers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
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
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
        }
    }

    /// The literal command name this agent's CLI spawns as. Infallible - there is no
    /// "this kind has no fixed binary" case to model, precisely because an `AgentKind` is never
    /// a shell (before the shell/agent split this was an `Option<&'static str>` every caller had
    /// to unwrap or `filter_map` away).
    ///
    /// Public so `crate::settings::state`'s Agents page can look up the same `$PATH` name this
    /// eventually hands `TerminalSpec::command` at spawn time (see [`ProcessKind::spec`]),
    /// instead of a second hardcoded list that could drift from what's actually spawned.
    pub fn binary_name(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        }
    }
}

/// What's actually running in an agent slot/tab: a bare interactive terminal, or a real agent
/// session. Purely descriptive - drives the tab label and which "New ..." button created it;
/// `TerminalPane` itself has no branching for "shell" vs. "agent CLI", and the PTY/tab/spawn/
/// close/focus machinery below is deliberately identical for both.
///
/// The shell/agent split is a *type-level* distinction, not a convention: a [`Self::Shell`] is a
/// bare terminal with no turns and nothing to review, so every `match` on this enum is forced by
/// the compiler to say what it does about that case. Cases that only ever cared about "is this a
/// shell or not" collapse to two arms (`Shell` / `Agent(_)`); the rarer cases that genuinely
/// differ per CLI destructure the inner [`AgentKind`]. And code that can only ever mean a real
/// agent doesn't take this type at all - it takes [`AgentKind`], which has no shell to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    Shell,
    Agent(AgentKind),
}

impl ProcessKind {
    /// Construction-only shorthand for `ProcessKind::Agent(AgentKind::Claude)`. Deliberately
    /// *not* usable to dodge exhaustiveness: it's a constructor, so a `match` still has to name
    /// the real variants and still breaks if a fourth distinguishing case is ever added.
    pub const fn claude() -> Self {
        ProcessKind::Agent(AgentKind::Claude)
    }

    /// Construction-only shorthand for `ProcessKind::Agent(AgentKind::Codex)` - see
    /// [`Self::claude`].
    pub const fn codex() -> Self {
        ProcessKind::Agent(AgentKind::Codex)
    }

    pub fn label(self) -> &'static str {
        match self {
            ProcessKind::Shell => "Shell",
            ProcessKind::Agent(agent) => agent.label(),
        }
    }

    /// `shell_override` is the user's configured shell
    /// (`crate::settings::store::TerminalSettings::shell_override`, GitHub issue #213) and only
    /// reaches [`ProcessKind::Shell`]: an agent CLI spawns its own fixed binary directly, never
    /// through a shell, so which shell the user prefers genuinely cannot affect it.
    ///
    /// `extras` is `(args, env)` to append for a real agent CLI - in practice
    /// `crate::hooks::HookRuntime::spawn_extras`'s `--settings <path>` plus the `JERRY_*`
    /// environment that lets that agent's hooks find their way home (GitHub issue #239 phase 2).
    /// It is deliberately handed in by the caller rather than derived here: whether hooks are
    /// available at all is a live, per-launch fact owned by `crate::root::AdeApp`, and this
    /// function is a pure `ProcessKind` -> `TerminalSpec` mapping that must stay testable without
    /// one.
    ///
    /// A [`ProcessKind::Shell`] discards `extras` outright. That is a real gate, not an
    /// oversight: a shell must never be handed the hook token or an agent id, because a shell is
    /// an interactive terminal running whatever the user types, and anything it ran would inherit
    /// the credentials for reporting status as some other pane.
    fn spec(
        self,
        cwd: PathBuf,
        shell_override: Option<&str>,
        extras: Option<SpawnExtras>,
    ) -> TerminalSpec {
        match self {
            ProcessKind::Shell => TerminalSpec::shell(cwd, shell_override),
            ProcessKind::Agent(agent) => {
                let (args, env) = extras.unwrap_or_default();
                TerminalSpec::command_with_env(agent.binary_name(), args, cwd, env)
            }
        }
    }

    /// Whether this slot holds a real agent session ([`ProcessKind::Agent`]) as opposed to
    /// [`ProcessKind::Shell`], which is just an interactive terminal that happens to live in the
    /// same rail/tab UI. The boolean form of that distinction, for filter/predicate call sites
    /// that don't need the inner [`AgentKind`]; anything that *does* need it should
    /// `if let ProcessKind::Agent(agent) = kind` (or match) rather than calling this and then
    /// re-deriving the CLI.
    pub fn is_agent_session(self) -> bool {
        matches!(self, ProcessKind::Agent(_))
    }
}

/// Extra spawn arguments and environment for one agent - `(args, env)`, as produced by
/// `crate::hooks::HookInjection::spawn_extras` and consumed by [`ProcessKind::spec`].
pub type SpawnExtras = (Vec<String>, Vec<(String, String)>);

impl From<AgentKind> for ProcessKind {
    fn from(agent: AgentKind) -> Self {
        ProcessKind::Agent(agent)
    }
}

/// A monotonically increasing agent id, stable across other agents closing - used
/// instead of a `Vec` index so a click handler capturing an id can't end up referring to the
/// wrong tab once some other tab closes and later indices shift down.
pub type AgentId = u64;

pub struct Agent {
    pub id: AgentId,
    pub kind: ProcessKind,
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
    /// real OS default", and it only affects [`ProcessKind::Shell`] tabs (see
    /// [`ProcessKind::spec`]).
    ///
    /// `hooks` is this launch's hook injection (GitHub issue #239 phase 2), or `None` when hook
    /// support isn't running - see [`crate::hooks::HookRuntime::start`] for every real reason it
    /// may not be. It is consumed *only* for [`AgentKind::Claude`]: hooks are a Claude Code
    /// feature, so a Codex agent and a shell are spawned byte-for-byte as they were before this
    /// phase, and both fall back to the terminal-title and quiescence signals.
    // Eight parameters, one past clippy's threshold. Every one is a distinct, live value read
    // fresh from settings or app state at each of this method's ~40 call sites (see the docs
    // above for why each is threaded rather than read here), so bundling them into a struct would
    // add a type whose only purpose is to be constructed inline at every call site and
    // immediately destructured here - moving the argument count rather than reducing it.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        &mut self,
        kind: ProcessKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        shell_override: Option<&str>,
        hooks: Option<&crate::hooks::HookInjection>,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        let id = self.next_id;
        self.next_id += 1;

        // Claude Code only, and gated here rather than inside `spec` so the gate is one
        // legible line next to the spawn it guards.
        let extras = match (kind, hooks) {
            (ProcessKind::Agent(AgentKind::Claude), Some(hooks)) => Some(hooks.spawn_extras(id)),
            _ => None,
        };
        let spec = kind.spec(cwd.clone(), shell_override, extras);
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

    /// Test-only: overrides an already-spawned agent's recorded [`ProcessKind`] without
    /// touching its real process. Exists for tests that need to exercise kind-gated logic (e.g.
    /// `crate::review::flow::AdeApp::capture_review_baseline`'s real-agent-only gate) without
    /// paying for an actual `claude`/`codex` CLI spawn - `ProcessKind::spec` only affects which
    /// binary [`Self::spawn`] execs, so retagging after the fact is honest: everything
    /// downstream of the recorded `kind` field behaves exactly as if a real agent CLI had been
    /// spawned, without the process weight of one.
    #[cfg(test)]
    pub(crate) fn set_kind_for_test(&mut self, id: AgentId, kind: ProcessKind) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == id) {
            agent.kind = kind;
        }
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

    #[test]
    fn every_agent_kind_is_a_session_and_a_shell_never_is() {
        // The whole point of the two-type split: `is_agent_session` can only ever be `false`
        // for the one variant that isn't an agent, and there is no `AgentKind` value at all
        // that could make it `false` - `ProcessKind::from` can't produce a shell.
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            assert!(
                ProcessKind::from(agent).is_agent_session(),
                "{agent:?} wrapped as a ProcessKind must be a real agent session"
            );
        }
        assert!(
            !ProcessKind::Shell.is_agent_session(),
            "a bare shell has no turns and nothing to review - it is never a session"
        );
    }

    #[test]
    fn the_construction_shorthands_are_exactly_their_long_forms() {
        // `claude()`/`codex()` exist only to keep call sites short; if they ever drifted from
        // the explicit construction, spawn sites would quietly disagree with match arms.
        assert_eq!(ProcessKind::claude(), ProcessKind::Agent(AgentKind::Claude));
        assert_eq!(ProcessKind::codex(), ProcessKind::Agent(AgentKind::Codex));
        assert_eq!(ProcessKind::claude(), AgentKind::Claude.into());
        assert_eq!(ProcessKind::codex(), AgentKind::Codex.into());
    }

    #[test]
    fn a_process_kinds_label_delegates_to_its_agents_own_label() {
        assert_eq!(ProcessKind::Shell.label(), "Shell");
        assert_eq!(ProcessKind::claude().label(), AgentKind::Claude.label());
        assert_eq!(ProcessKind::codex().label(), AgentKind::Codex.label());
        assert_eq!(AgentKind::Claude.label(), "Claude");
        assert_eq!(AgentKind::Codex.label(), "Codex");
    }

    #[test]
    fn each_agent_kind_spawns_its_own_distinct_path_binary() {
        assert_eq!(AgentKind::Claude.binary_name(), "claude");
        assert_eq!(AgentKind::Codex.binary_name(), "codex");
        assert_ne!(
            AgentKind::Claude.binary_name(),
            AgentKind::Codex.binary_name(),
            "a copy-pasted arm sharing one binary name would spawn the wrong CLI silently"
        );
    }

    #[test]
    fn a_shell_never_receives_the_hook_injection_even_when_one_is_offered() {
        // A shell runs whatever the user types, so handing it the hook token and an agent id
        // would let anything it ran report status as some other pane. `spec` discards `extras`
        // for the shell arm outright - this pins that.
        let extras = Some((
            vec!["--settings".to_owned(), "/tmp/jerry.json".to_owned()],
            vec![("JERRY_HOOK_TOKEN".to_owned(), "secret".to_owned())],
        ));
        let shell = ProcessKind::Shell.spec(PathBuf::from("/tmp"), None, extras);
        assert!(shell.args.is_empty(), "a shell must get no injected args");
        assert!(
            shell.env.is_empty(),
            "a shell must never see the hook token"
        );
    }

    #[test]
    fn an_agent_given_a_hook_injection_really_spawns_with_it() {
        // The other half: the injection must actually reach the spawn, or the whole side-channel
        // silently does nothing.
        let extras = Some((
            vec!["--settings".to_owned(), "/tmp/jerry.json".to_owned()],
            vec![("JERRY_HOOK_PORT".to_owned(), "5000".to_owned())],
        ));
        let spec = ProcessKind::claude().spec(PathBuf::from("/tmp"), None, extras);
        assert_eq!(spec.program, PathBuf::from("claude"));
        assert_eq!(spec.args, vec!["--settings", "/tmp/jerry.json"]);
        assert_eq!(
            spec.env,
            vec![("JERRY_HOOK_PORT".to_owned(), "5000".to_owned())]
        );
    }

    /// `spec` is private, but the fact it reads through `AgentKind::binary_name` (rather than a
    /// second hardcoded list) is what keeps the Settings › Agents `$PATH` search honest: the name
    /// that card searches for must be the name actually spawned. This asserts the shared source,
    /// which is the part that could drift.
    #[test]
    fn the_settings_path_search_and_the_real_spawn_read_the_same_binary_name() {
        for agent in [AgentKind::Claude, AgentKind::Codex] {
            let spec = ProcessKind::Agent(agent).spec(PathBuf::from("/tmp"), None, None);
            assert_eq!(
                spec.program,
                PathBuf::from(agent.binary_name()),
                "{agent:?} must spawn exactly the binary the Agents page searches $PATH for"
            );
            assert!(
                spec.args.is_empty(),
                "{agent:?} with no hook injection is spawned bare, exactly as before issue #239"
            );
            assert!(
                spec.env.is_empty(),
                "{agent:?} with no hook injection gets no extra environment either"
            );
        }
        // `shell_override` reaches only the shell arm - an agent CLI is spawned directly, never
        // through a shell, so the user's preferred shell genuinely cannot affect it.
        let shell = ProcessKind::Shell.spec(PathBuf::from("/tmp"), Some("/bin/dash"), None);
        assert_eq!(
            shell.program,
            PathBuf::from("/bin/dash"),
            "a shell resolves the user's configured shell, not a fixed agent binary"
        );
        let agent_ignoring_override =
            ProcessKind::claude().spec(PathBuf::from("/tmp"), Some("/bin/dash"), None);
        assert_eq!(
            agent_ignoring_override.program,
            PathBuf::from("claude"),
            "a configured shell must not be substituted for an agent CLI's own binary"
        );
    }
}
