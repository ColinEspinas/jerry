//! Agent/tab bookkeeping: ties a spawned terminal process (a [`TerminalPane`]) to the
//! worktree it's running in, and tracks which agents are open and which one is active for
//! the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{App, AppContext as _, Context, Entity, Focusable as _, Subscription, Window};

use crate::root::AdeApp;
use crate::terminal::pane::{TerminalPane, TerminalPaneEvent, TerminalSpec};

/// Which agent CLI a real agent runs. Never a bare shell - see [`ProcessKind`] for the type that
/// also covers a plain interactive terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    /// The `claude` CLI (Claude Code), spawned with no arguments in the chosen worktree.
    /// Resolved via `PATH`; if not installed, spawning fails and the pane shows
    /// `TerminalPane::spawn_error`.
    Claude,
    /// The `codex` CLI, spawned the same way as [`AgentKind::Claude`].
    Codex,
    /// The Cursor agent CLI (GitHub issue #463), spawned the same way as [`AgentKind::Claude`].
    /// Its binary is `cursor-agent`, not `cursor` - the latter is the editor.
    Cursor,
}

impl AgentKind {
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
            AgentKind::Cursor => "Cursor",
        }
    }

    /// The inverse of [`Self::label`] - `None` for anything this build doesn't recognise, which a
    /// reader of persisted data (`crate::hooks::store::PersistedAgentStatus::kind`, GitHub issue
    /// #227) must treat as "this record isn't usable" rather than guessing a kind.
    pub fn from_label(label: &str) -> Option<AgentKind> {
        match label {
            "Claude" => Some(AgentKind::Claude),
            "Codex" => Some(AgentKind::Codex),
            "Cursor" => Some(AgentKind::Cursor),
            _ => None,
        }
    }

    /// The literal command name this agent's CLI spawns as. Infallible - there is no
    /// "this kind has no fixed binary" case to model, precisely because an `AgentKind` is never
    /// a shell (before the shell/agent split this was an `Option<&'static str>` every caller had
    /// to unwrap or `filter_map` away).
    pub fn binary_name(self) -> &'static str {
        match self {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
            AgentKind::Cursor => "cursor-agent",
        }
    }

    /// The argument vector that makes this CLI reattach to `session_id`, or `None` for a kind
    /// with no resume flag at all - the one place the two spellings live, so a call site can't
    /// pick the wrong one. Both were checked against a real binary, and both genuinely resume the
    /// named conversation rather than merely starting a fresh one in the same directory:
    /// `claude` 2.1.228 (see `crate::hooks::event::HookReport::session_id`), and `cursor-agent`
    /// 2026.08.11-e8db854, whose `--resume=<id>` recalled a token set by an earlier, separate
    /// process.
    ///
    /// The spellings really do differ: `claude` takes the id as its own argv entry, while
    /// `cursor-agent`'s `--resume [chatId]` is an *optional*-value option, so a detached
    /// `--resume <id>` there means "prompt me to pick a session" and treats the id as a prompt.
    pub fn resume_args(self, session_id: &str) -> Option<Vec<String>> {
        match self {
            AgentKind::Claude => Some(vec!["--resume".to_owned(), session_id.to_owned()]),
            AgentKind::Cursor => Some(vec![format!("--resume={session_id}")]),
            // `codex` has no resume flag this app has verified, so it never claims one.
            AgentKind::Codex => None,
        }
    }

    /// Whether this CLI can mint a chat id up front, for a kind whose id cannot arrive any other
    /// way. `claude` learns its own `session_id` from its hooks after the fact; `cursor-agent` is
    /// hookless here (see `crate::hooks::flow::AdeApp::hook_injection_for`), so its id has to be
    /// created before the spawn that uses it - `cursor-agent create-chat` prints a bare UUID for
    /// exactly this.
    pub fn mints_chat_id(self) -> bool {
        matches!(self, AgentKind::Cursor)
    }
}

/// What's actually running in an agent slot/tab: a bare interactive terminal, or a real agent
/// session. Purely descriptive - drives the tab label and which "New ..." button created it;
/// `TerminalPane` itself has no branching for "shell" vs. "agent CLI", and the PTY/tab/spawn/
/// close/focus machinery below is deliberately identical for both.
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

    /// Construction-only shorthand for `ProcessKind::Agent(AgentKind::Cursor)` - see
    /// [`Self::claude`].
    pub const fn cursor() -> Self {
        ProcessKind::Agent(AgentKind::Cursor)
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
    /// When this agent was spawned - the rail agent row's real elapsed-time trailing text.
    /// Set once, here, and never mutated afterwards - this app has no
    /// pause/resume-with-a-fresh-clock concept (see `crate::work_surface::state::pty_state_label`'s
    /// docs), so "elapsed" is simply wall-clock time since this real process was spawned.
    pub spawned_at: Instant,
    /// The same spawn moment as [`Self::spawned_at`], but as real wall-clock seconds since the
    /// Unix epoch - set from the identical call site, at the identical instant.
    pub spawned_at_unix: i64,
    /// The conversation this pane is attached to, for a kind that cannot learn one from hooks.
    /// `None` for `AgentKind::Claude`, whose id arrives later on the hook side-channel and is
    /// read from there (`crate::hooks::HookRuntime::session_id_for`) rather than stored here, and
    /// `None` for any Cursor pane whose id could not be minted at spawn time - an agent with no
    /// id is simply not resumable, which is what the run-history footer already says.
    pub session_id: Option<String>,
    /// Keeps [`Agents::spawn`]'s subscription to this agent's pane (see [`TerminalPaneEvent`])
    /// alive for this agent's lifetime - never read, only held.
    _pane_subscription: Subscription,
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

    /// How many **real agent sessions** are currently open in `cwd` - the real signal behind
    /// GitHub issue #225's **single-agent gate**.
    pub fn count_for_cwd(&self, cwd: &Path) -> usize {
        self.iter_for_cwd(cwd.to_path_buf())
            .filter(|agent| agent.kind.is_agent_session())
            .count()
    }

    /// `true` if `id` names a **real agent session** that is the only one currently open in its
    /// own worktree: the review surface's real gate, in the one form every call site wants.
    /// `false` for an unknown id (e.g. a just-closed agent), so a stale click handler can never
    /// re-open a review surface for an agent that no longer exists - and `false` for a plain
    /// [`ProcessKind::Shell`], which is not a session this gate can ever open *for* (it has no
    /// baseline and nothing to review); without that check a lone shell would satisfy a gate
    /// whose whole subject is "which agent's changes are these".
    pub fn is_sole_agent_in_worktree(&self, id: AgentId) -> bool {
        let Some(agent) = self.agents.iter().find(|agent| agent.id == id) else {
            return false;
        };
        agent.kind.is_agent_session() && self.count_for_cwd(&agent.cwd) == 1
    }

    /// **The worktree's primary agent**: its own last-active tab (see [`Self::active_by_cwd`]),
    /// or - for a worktree that has never had one recorded - its first open agent in creation
    /// order. `None` if the worktree has no open agents at all.
    pub fn primary_for_cwd(&self, cwd: &Path) -> Option<&Agent> {
        let remembered = self
            .active_by_cwd
            .get(cwd)
            .copied()
            .and_then(|id| self.agents.iter().find(|agent| agent.id == id));
        remembered.or_else(|| self.agents.iter().find(|agent| agent.cwd == cwd))
    }

    pub fn active_id(&self) -> Option<AgentId> {
        self.active
    }

    pub fn active(&self) -> Option<&Agent> {
        let id = self.active?;
        self.agents.iter().find(|agent| agent.id == id)
    }

    /// Spawns a new agent of `kind` into `cwd` (the caller resolves this - see
    /// `crate::root::AdeApp::current_worktree_path`), appends it as a new tab, and makes it
    /// active (both globally, [`Self::active`], and as `cwd`'s own remembered tab,
    /// [`Self::active_by_cwd`]). Returns the new agent's id.
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
        self.spawn_inner(
            kind,
            cwd,
            terminal_font_size_px,
            shell_override,
            hooks,
            Vec::new(),
            window,
            cx,
        )
    }

    /// [`Self::spawn`], but for a real Claude Code `--resume <session_id>` (GitHub issue #227):
    /// spawns `agent_kind` into `cwd` exactly as a fresh agent would, plus `--resume
    /// <session_id>` prepended to its arguments - verified against a real `claude` 2.1.228 binary
    /// to genuinely pick the named conversation back up, not merely start a new one in the same
    /// directory (`crate::hooks::event::HookReport::session_id`'s own docs cover how that was
    /// checked).
    // Nine parameters, matching `Self::spawn`'s own already-`#[allow]`'d count plus the one real
    // addition (`session_id`) - see that method's docs for why bundling the rest into a struct
    // wouldn't reduce anything.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_resume(
        &mut self,
        agent_kind: AgentKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        shell_override: Option<&str>,
        hooks: Option<&crate::hooks::HookInjection>,
        session_id: String,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        // A kind with no verified resume flag ([`AgentKind::resume_args`]) is spawned bare rather
        // than handed an invented one - it is then simply a fresh agent in the same worktree,
        // which is what the run-history footer already tells the user it will be.
        let leading_args = agent_kind.resume_args(&session_id).unwrap_or_default();
        let id = self.spawn_inner(
            ProcessKind::Agent(agent_kind),
            cwd,
            terminal_font_size_px,
            shell_override,
            hooks,
            leading_args,
            window,
            cx,
        );
        // A hookless kind has no other way to report which conversation this pane is - see
        // [`Agent::session_id`].
        self.set_session_id(id, session_id);
        id
    }

    /// Records the conversation id a pane is attached to, for a kind that cannot learn one from
    /// hooks - see [`Agent::session_id`]. A no-op for an id that isn't open.
    pub fn set_session_id(&mut self, id: AgentId, session_id: String) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == id) {
            agent.session_id = Some(session_id);
        }
    }

    /// The conversation id this pane is attached to, if it was known at spawn time - `None` for
    /// every kind whose id arrives through hooks instead (see [`Agent::session_id`]).
    pub fn session_id_for(&self, id: AgentId) -> Option<&str> {
        self.agents
            .iter()
            .find(|agent| agent.id == id)?
            .session_id
            .as_deref()
    }

    /// The real shared core of [`Self::spawn`]/[`Self::spawn_resume`] - identical in every way
    /// except which extra CLI arguments (if any) are prepended ahead of the hook injection's own
    /// `--settings <path>`. Kept private and `#[allow(clippy::too_many_arguments)]`'d here rather
    /// than on both public callers, so the many "no extra args" call sites through [`Self::spawn`]
    /// keep their existing, unchanged arity.
    #[allow(clippy::too_many_arguments)]
    fn spawn_inner(
        &mut self,
        kind: ProcessKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        shell_override: Option<&str>,
        hooks: Option<&crate::hooks::HookInjection>,
        mut leading_args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        let id = self.next_id;
        self.next_id += 1;

        // Claude Code only, and gated here rather than inside `spec` so the gate is one
        // legible line next to the spawn it guards.
        let hook_extras = match (kind, hooks) {
            (ProcessKind::Agent(AgentKind::Claude), Some(hooks)) => hooks.spawn_extras(id),
            _ => None,
        };
        let extras = if leading_args.is_empty() {
            hook_extras
        } else {
            let (mut hook_args, env) = hook_extras.unwrap_or_default();
            leading_args.append(&mut hook_args);
            Some((leading_args, env))
        };
        let spec = kind.spec(cwd.clone(), shell_override, extras);
        let pane = cx.new(|cx| TerminalPane::new(spec, terminal_font_size_px, cx));
        let pane_subscription = cx.subscribe_in(
            &pane,
            window,
            move |app, _pane, event, window, cx| match event {
                TerminalPaneEvent::OpenPath { path, line } => {
                    app.open_terminal_link(path.clone(), *line, window, cx);
                }
                // A process the user asked to quit takes its tab with it; a crashed or killed
                // one keeps its pane so its last output, and the footer's Retry/Resume, are
                // still there to read.
                TerminalPaneEvent::ProcessExited { clean } => {
                    if *clean {
                        app.close_agent(id, window, cx);
                    }
                }
            },
        );

        self.agents.push(Agent {
            id,
            kind,
            cwd: cwd.clone(),
            pane,
            spawned_at: Instant::now(),
            spawned_at_unix: unix_now(),
            session_id: None,
            _pane_subscription: pane_subscription,
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

    /// Test-only sibling of [`Self::set_kind_for_test`]: overrides an already-spawned agent's
    /// recorded spawn second without touching its real process.
    #[cfg(test)]
    pub(crate) fn set_spawned_at_unix_for_test(&mut self, id: AgentId, spawned_at_unix: i64) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == id) {
            agent.spawned_at_unix = spawned_at_unix;
        }
    }

    /// Real `SIGSTOP`, via `TerminalPane::pause`, against every real agent session (never a bare
    /// [`ProcessKind::Shell`] - mirrors [`Self::count_for_cwd`]'s own "a shell has no turns and
    /// nothing to review" convention) currently open in `cwd` - GitHub issue #242 phase B's
    /// interactive-rebase UI's "Pause now" action, freezing whatever coding agents are running in
    /// this worktree before a rebase rewrites files out from under them. Returns exactly the ids
    /// this call really paused (skipping any [`TerminalPane::pause`] that itself failed, e.g. no
    /// live session yet, or - honestly reported and simply not counted as paused - a platform
    /// with no `SIGSTOP` at all), so the caller can later [`Self::resume_agents`] precisely that
    /// set rather than guessing.
    pub fn pause_agents_for_cwd(&self, cwd: &Path, cx: &mut Context<AdeApp>) -> Vec<AgentId> {
        self.iter_for_cwd(cwd.to_path_buf())
            .filter(|agent| agent.kind.is_agent_session())
            .map(|agent| agent.id)
            .collect::<Vec<_>>()
            .into_iter()
            .filter(|id| {
                let Some(agent) = self.agents.iter().find(|agent| agent.id == *id) else {
                    return false;
                };
                agent.pane.read(cx).pause().is_ok()
            })
            .collect()
    }

    /// Real `SIGCONT`, via `TerminalPane::resume`, against every id in `ids` still open - the
    /// counterpart to [`Self::pause_agents_for_cwd`]. Silently skips any id that has since
    /// closed (its process is already gone; there is nothing left to resume) rather than
    /// erroring - resuming is always a best-effort "undo the earlier pause", never itself a
    /// user-facing action that can fail.
    pub fn resume_agents(&self, ids: &[AgentId], cx: &mut Context<AdeApp>) {
        for id in ids {
            if let Some(agent) = self.agents.iter().find(|agent| agent.id == *id) {
                if let Err(err) = agent.pane.read(cx).resume() {
                    log::warn!("failed to resume agent {id}: {err}");
                }
            }
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
    pub fn activate_for_worktree(&mut self, cwd: &Path, cx: &mut Context<AdeApp>) {
        let id = self.primary_for_cwd(cwd).map(|agent| agent.id);
        self.active = id;
        if let Some(id) = id {
            self.active_by_cwd.insert(cwd.to_path_buf(), id);
        }
        self.sync_pane_cadence(cx);
    }

    /// The other half of [`Self::activate_for_worktree`]'s own worktree-scoping fix: clears
    /// [`Self::active`] outright, with no worktree to fall back into. `crate::root::AdeApp::
    /// checkout_repo_from_rail` is the one real caller - focusing a repo from the rail is a pure
    /// navigation gesture (which repo's worktrees the rail shows), never a worktree selection, so
    /// nothing here may resolve to *some* agent the way `activate_for_worktree` does. Not calling
    /// this at all was tried first and was itself the bug: [`Self::active`] would then still
    /// point at whichever agent was active in the repo just *left*, and since [`Self::active`] is
    /// read directly by the centre pane with no repo-scoping of its own
    /// (`crate::work_surface::render::AdeApp::render_center_pane`), that repo's terminal kept
    /// rendering right alongside the newly-focused repo's own rail rows - a real, worse
    /// inconsistency than the one this exists to fix. `active_by_cwd`'s own remembered-tab
    /// entries are untouched, so a later real worktree-row click still lands on the same agent
    /// [`Self::activate_for_worktree`] would have picked before this call ever ran.
    pub fn clear_active(&mut self, cx: &mut Context<AdeApp>) {
        self.active = None;
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
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Cursor] {
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
    fn each_kinds_resume_spelling_is_the_one_its_own_cli_accepts() {
        // The two spellings are not interchangeable, and neither is guessed: `claude` 2.1.228
        // takes the id as a separate argv entry, while `cursor-agent`'s `--resume [chatId]` is an
        // optional-value option, so a detached id there is read as a prompt rather than a chat.
        assert_eq!(
            AgentKind::Claude.resume_args("abc"),
            Some(vec!["--resume".to_owned(), "abc".to_owned()])
        );
        assert_eq!(
            AgentKind::Cursor.resume_args("abc"),
            Some(vec!["--resume=abc".to_owned()]),
            "cursor-agent must receive one argv entry, not two"
        );
        assert_eq!(
            AgentKind::Codex.resume_args("abc"),
            None,
            "a kind with no verified resume flag must never claim one"
        );
    }

    #[test]
    fn only_a_hookless_kind_mints_its_own_chat_id() {
        // Claude's id arrives on the hook side-channel after the fact, so minting one for it
        // would attach the pane to a conversation its own CLI knows nothing about.
        assert!(AgentKind::Cursor.mints_chat_id());
        assert!(!AgentKind::Claude.mints_chat_id());
        assert!(!AgentKind::Codex.mints_chat_id());
    }

    #[test]
    fn a_kind_that_mints_an_id_can_always_spend_it() {
        // The two halves have to agree: minting an id for a kind that has no resume flag would
        // pay for a chat id and then spawn without it.
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Cursor] {
            if kind.mints_chat_id() {
                assert!(
                    kind.resume_args("some-id").is_some(),
                    "{kind:?} mints a chat id but has no way to pass one"
                );
            }
        }
    }

    #[test]
    fn the_construction_shorthands_are_exactly_their_long_forms() {
        // `claude()`/`codex()` exist only to keep call sites short; if they ever drifted from
        // the explicit construction, spawn sites would quietly disagree with match arms.
        assert_eq!(ProcessKind::claude(), ProcessKind::Agent(AgentKind::Claude));
        assert_eq!(ProcessKind::codex(), ProcessKind::Agent(AgentKind::Codex));
        assert_eq!(ProcessKind::cursor(), ProcessKind::Agent(AgentKind::Cursor));
        assert_eq!(ProcessKind::claude(), AgentKind::Claude.into());
        assert_eq!(ProcessKind::codex(), AgentKind::Codex.into());
        assert_eq!(ProcessKind::cursor(), AgentKind::Cursor.into());
    }

    #[test]
    fn a_process_kinds_label_delegates_to_its_agents_own_label() {
        assert_eq!(ProcessKind::Shell.label(), "Shell");
        assert_eq!(ProcessKind::claude().label(), AgentKind::Claude.label());
        assert_eq!(ProcessKind::codex().label(), AgentKind::Codex.label());
        assert_eq!(ProcessKind::cursor().label(), AgentKind::Cursor.label());
        assert_eq!(AgentKind::Claude.label(), "Claude");
        assert_eq!(AgentKind::Codex.label(), "Codex");
        assert_eq!(AgentKind::Cursor.label(), "Cursor");
    }

    #[test]
    fn from_label_is_the_real_inverse_of_label_for_every_agent_kind() {
        // GitHub issue #227's history reader (`crate::hooks::history`) decodes a persisted
        // `PersistedAgentStatus::kind` string back into a real `AgentKind` through this - it must
        // agree with `label` exactly, or a real persisted record would silently fail to decode.
        for kind in [AgentKind::Claude, AgentKind::Codex, AgentKind::Cursor] {
            assert_eq!(AgentKind::from_label(kind.label()), Some(kind));
        }
        assert_eq!(
            AgentKind::from_label("SomeFutureAgent"),
            None,
            "an unrecognised label must be `None`, never a guessed kind"
        );
    }

    #[test]
    fn each_agent_kind_spawns_its_own_distinct_path_binary() {
        assert_eq!(AgentKind::Claude.binary_name(), "claude");
        assert_eq!(AgentKind::Codex.binary_name(), "codex");
        // `cursor-agent`, not `cursor`: the latter is the Cursor editor's own launcher, so a
        // slip here would spawn a GUI editor where an agent session was asked for.
        assert_eq!(AgentKind::Cursor.binary_name(), "cursor-agent");
        let kinds = [AgentKind::Claude, AgentKind::Codex, AgentKind::Cursor];
        for (index, kind) in kinds.iter().enumerate() {
            for other in &kinds[index + 1..] {
                assert_ne!(
                    kind.binary_name(),
                    other.binary_name(),
                    "a copy-pasted arm sharing one binary name would spawn the wrong CLI silently"
                );
            }
        }
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

    #[test]
    fn the_settings_path_search_and_the_real_spawn_read_the_same_binary_name() {
        for agent in [AgentKind::Claude, AgentKind::Codex, AgentKind::Cursor] {
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

/// How long [`AdeApp::spawn_with_minted_chat_id`] waits for a chat id before giving up and
/// spawning a plain, unresumable agent. Generous because minting is a real network round-trip,
/// bounded because `cursor-agent create-chat` hangs forever rather than failing when the user is
/// not logged in.
const CHAT_ID_MINT_TIMEOUT: Duration = Duration::from_secs(15);

/// Whether a line a CLI printed can be taken as a conversation id rather than a message it wrote
/// to stdout instead. Shape-only - the real check is that the resumed agent attaches, which only
/// the CLI itself can decide.
fn is_plausible_chat_id(line: &str) -> bool {
    !line.is_empty()
        && line.len() <= 128
        && line
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

impl AdeApp {
    /// Spawns a kind that has to be handed its conversation id up front
    /// ([`AgentKind::mints_chat_id`]): mints one off the UI thread, then spawns attached to it.
    ///
    /// A mint that fails - no binary, not logged in, or the timeout - falls back to a plain bare
    /// spawn. That agent is then simply not resumable, which the run-history footer already
    /// reports honestly rather than offering a resume that would start a fresh chat instead.
    pub(crate) fn spawn_with_minted_chat_id(
        &mut self,
        agent_kind: AgentKind,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let font_size = self.settings.appearance.terminal_font_size;
        let shell_override = self.settings.terminal.shell_override().map(str::to_owned);
        let binary = agent_kind.binary_name();
        let mint_cwd = cwd.clone();
        let task = cx.spawn(async move |this, cx| {
            let chat_id = cx
                .background_executor()
                .spawn(async move { mint_chat_id(binary, &mint_cwd) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                let hook_injection = this.hook_injection_for(ProcessKind::Agent(agent_kind));
                let id = match chat_id {
                    Some(chat_id) => this.agents.spawn_resume(
                        agent_kind,
                        cwd,
                        font_size,
                        shell_override.as_deref(),
                        hook_injection.as_ref(),
                        chat_id,
                        window,
                        cx,
                    ),
                    None => this.agents.spawn(
                        ProcessKind::Agent(agent_kind),
                        cwd,
                        font_size,
                        shell_override.as_deref(),
                        hook_injection.as_ref(),
                        window,
                        cx,
                    ),
                };
                this.after_agent_spawn(id, window, cx);
            });
        });
        // Same `TaskPool` reasoning as `AdeApp::new_agent_pane`: two rapid clicks must each get
        // their own agent rather than the second cancelling the first.
        self._new_agent_pane_task.push(task);
    }
}

/// Runs the CLI's own "give me a new chat id" command, or `None` if it could not produce one.
///
/// Blocking - callers run it on the background executor.
fn mint_chat_id(binary: &str, cwd: &Path) -> Option<String> {
    // Tests never mint: this is a real subprocess making a real network call, and a UI test that
    // reached it would hang on the machine of anyone who has the CLI installed. The fallback it
    // takes instead is the same path production takes when minting fails, so the spawn stays
    // covered either way, and the capture helper is tested directly in its own crate.
    if cfg!(test) {
        return None;
    }
    match pty_core::capture_first_line(binary, &["create-chat"], cwd, CHAT_ID_MINT_TIMEOUT) {
        Ok(line) if is_plausible_chat_id(&line) => Some(line),
        Ok(line) => {
            log::warn!("`{binary} create-chat` printed {line:?}, not a chat id");
            None
        }
        Err(err) => {
            log::warn!("could not mint a {binary} chat id ({err}) - this agent starts unresumable");
            None
        }
    }
}

#[cfg(test)]
mod chat_id_tests {
    use super::is_plausible_chat_id;

    #[test]
    fn a_real_minted_id_is_accepted() {
        // The exact shape `cursor-agent create-chat` really printed.
        assert!(is_plausible_chat_id("51a6b5fa-fbf4-4116-b44c-cd3e0aa35a5e"));
    }

    #[test]
    fn a_message_printed_instead_of_an_id_is_refused() {
        // A CLI that reports a failure on stdout must not have its prose spliced into an argv as
        // though it were a conversation id.
        for line in [
            "",
            "Not logged in",
            "error: authentication required",
            "You must run `cursor-agent login` first",
            "--resume=x; rm -rf /",
        ] {
            assert!(
                !is_plausible_chat_id(line),
                "{line:?} must not be treated as a chat id"
            );
        }
    }

    #[test]
    fn an_absurdly_long_line_is_refused() {
        assert!(!is_plausible_chat_id(&"a".repeat(129)));
    }
}
