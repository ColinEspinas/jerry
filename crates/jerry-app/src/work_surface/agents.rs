//! Agent/tab bookkeeping: ties a spawned terminal process (a [`TerminalPane`]) to the
//! worktree it's running in, and tracks which agents are open and which one is active for
//! the tabbed center pane. `TerminalPane` itself has no notion of tabs or of "which
//! worktree" - see its module docs - this is that one layer up.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Focusable as _, Subscription, WeakEntity,
    Window,
};

use crate::root::AdeApp;
use crate::terminal::pane::{
    SessionAdapter, TerminalPane, TerminalPaneEvent, TerminalSpec, TERMINAL_COLS, TERMINAL_ROWS,
};
use jerry_core::{AppCommand, Report, Request, SessionSpawn};

/// Which agent CLI a real agent runs. Never a bare shell - see [`ProcessKind`] for the type that
/// also covers a plain interactive terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
                let env = with_jerry_on_path(env, jerry_core::jerry_binary::locate);
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

/// [`Agent::spawn_inner`]'s hook-injection gate, split out as a pure function so it's directly
/// unit-testable without a real `TerminalPane` spawn. Claude gets `--settings <path>` plus env
/// (`HookInjection::spawn_extras`); Cursor (GitHub issue #479) has no such flag, so it gets only
/// the env triplet (`HookInjection::env_only`) and no extra leading argument - and only when
/// `hooks` is `Some` at all, which `crate::hooks::flow::AdeApp::hook_injection_for` already
/// withholds unless the user has opted in via the `cursor_hooks_enabled` setting. Every other
/// kind (`Codex`, or any kind with no injection offered) gets no extras.
fn hook_extras_for(
    kind: ProcessKind,
    hooks: Option<&crate::hooks::HookInjection>,
    id: AgentId,
) -> Option<SpawnExtras> {
    match (kind, hooks) {
        (ProcessKind::Agent(AgentKind::Claude), Some(hooks)) => hooks.spawn_extras(id),
        (ProcessKind::Agent(AgentKind::Cursor), Some(hooks)) => {
            Some((Vec::new(), hooks.env_only(id)))
        }
        _ => None,
    }
}

/// Prepends the directory holding the `jerry` binary to `env`'s `PATH`, so every spawned agent
/// can find `jerry` with no injected flag - the one thing an agent needs to know the CLI exists
/// at all (`docs/architecture/decisions.md` §21). `locate` is injected so a test can supply a
/// fake sibling directory without a real binary on disk; production passes
/// [`jerry_core::jerry_binary::locate`]. A `locate` miss logs a warning and leaves `env`
/// untouched - PATH injection is a convenience, never something that may block a spawn.
fn with_jerry_on_path(
    mut env: Vec<(String, String)>,
    locate: impl FnOnce() -> Option<PathBuf>,
) -> Vec<(String, String)> {
    let Some(jerry_binary) = locate() else {
        log::warn!(
            "jerry-app: could not locate the jerry binary; a spawned agent's PATH will not \
             include it"
        );
        return env;
    };
    let Some(dir) = jerry_binary.parent() else {
        return env;
    };
    match prepend_dir_to_path(dir, std::env::var_os("PATH").as_deref()) {
        Ok(joined) => match joined.to_str() {
            Some(joined) => env.push(("PATH".to_owned(), joined.to_owned())),
            None => log::warn!(
                "jerry-app: the PATH with {} prepended is not valid UTF-8; leaving this spawn's \
                 PATH unset",
                dir.display()
            ),
        },
        Err(error) => {
            log::warn!("jerry-app: could not build a PATH for a spawned agent: {error}")
        }
    }
    env
}

/// `dir`, then every entry of `existing` (Windows joins with `;`, Unix with `:` -
/// [`std::env::join_paths`] picks the right one for this platform).
fn prepend_dir_to_path(
    dir: &Path,
    existing: Option<&std::ffi::OsStr>,
) -> Result<std::ffi::OsString, std::env::JoinPathsError> {
    let mut entries = vec![dir.to_path_buf()];
    if let Some(existing) = existing {
        entries.extend(std::env::split_paths(existing));
    }
    std::env::join_paths(entries)
}

impl From<AgentKind> for ProcessKind {
    fn from(agent: AgentKind) -> Self {
        ProcessKind::Agent(agent)
    }
}

/// The dispatch-boundary conversion `docs/architecture/decisions.md` §21 calls for:
/// `jerry_core::AgentSpec` is jerry-core's own independent mirror of these three kinds (jerry-core
/// cannot depend on jerry-app), reunited here into a real [`AgentKind`] wherever the app acts on
/// an `event/worktree-created` notification's `agent` field. Exhaustive, so a third kind added to
/// either enum without the other is a compile error, not a silently wrong spawn.
impl From<jerry_core::AgentSpec> for AgentKind {
    fn from(spec: jerry_core::AgentSpec) -> Self {
        match spec {
            jerry_core::AgentSpec::Claude => AgentKind::Claude,
            jerry_core::AgentSpec::Codex => AgentKind::Codex,
            jerry_core::AgentSpec::Cursor => AgentKind::Cursor,
        }
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
    /// The host's own PTY session id for this agent's process, once `Agents::spawn_inner`'s
    /// `SessionSpawn` dispatch resolves - distinct from [`Self::session_id`] (a CLI conversation
    /// id). `None` until attach, and briefly `None` again for a spawn that failed. What
    /// `crate::work_surface::session_exited` looks an incoming `event/session-exited` up by.
    pub host_session_id: Option<jerry_core::SessionId>,
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
    /// The host's table of agents, once the host is up. Every agent spawn registers here and
    /// every close forgets, so `jerry` calls from an agent classify as that agent and stay
    /// confined to its worktree. Plain shells carry no `JERRY_AGENT_ID` and are never entered.
    host_agents: Option<jerry_host::AgentTable>,
    /// Test-only per-kind spawn override - see [`Self::override_binary`]. Always empty in
    /// production, since nothing outside `#[cfg(test)]` code ever inserts into it.
    binary_overrides: HashMap<AgentKind, (PathBuf, Vec<String>)>,
    /// [`Self::spawn_inner`]'s own `SessionSpawn`-dispatch-then-attach task, one per spawn - held
    /// here (rather than detached) so it is cancelled, not orphaned, if this collection (and so
    /// the `AdeApp` that owns it) drops before a spawn resolves.
    _spawn_tasks: crate::root::task_pool::TaskPool,
    /// [`Self::close`]'s own doom-poll-shutdown task, one per close - the same lifecycle
    /// reasoning as [`Self::_spawn_tasks`], kept in a separate pool since it is a different
    /// concern (`crate::root::task_pool::TaskPool`'s own docs list several such pools on
    /// `AdeApp` for exactly this reason).
    _close_tasks: crate::root::task_pool::TaskPool,
    /// Host session ids an `event/session-exited` notification named before
    /// [`Self::set_host_session_id`] ever recorded which agent that id belongs to - real on
    /// Linux, where `sh -c exit` can finish inside the very dispatch round trip that spawned it,
    /// so its exit event reaches this instance's subscriber before the spawn's own response has
    /// even resolved (`crate::work_surface::session_exited`'s own docs). Consulted, and cleared
    /// of any match, by [`Self::set_host_session_id`] the moment the id it was waiting for
    /// finally arrives. An id belonging to a session this instance never spawns at all (another
    /// client's session on the same host) is never claimed and stays here - accepted, since the
    /// fanout only ever names a session id once per exit and each entry is a small string.
    pending_exits: HashSet<jerry_core::SessionId>,
}

impl Agents {
    pub fn new() -> Self {
        Self {
            agents: Vec::new(),
            active: None,
            active_by_cwd: HashMap::new(),
            next_id: 0,
            host_agents: None,
            binary_overrides: HashMap::new(),
            _spawn_tasks: crate::root::task_pool::TaskPool::default(),
            _close_tasks: crate::root::task_pool::TaskPool::default(),
            pending_exits: HashSet::new(),
        }
    }

    /// Hands the host every agent already open and every one spawned from now on.
    pub fn attach_host(&mut self, table: jerry_host::AgentTable) {
        for agent in &self.agents {
            if let ProcessKind::Agent(kind) = agent.kind {
                table.register(
                    host_agent_id(agent.id),
                    agent.cwd.clone(),
                    kind.label().to_owned(),
                );
            }
        }
        self.host_agents = Some(table);
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

    /// [`Self::spawn`], but with `prompt` as the agent CLI's own leading positional argument -
    /// `claude`/`codex`'s real "start with this initial message" convention, the same one a
    /// human types at the end of the command line. For `event/worktree-created`'s own agent
    /// spawn (`docs/architecture/decisions.md` §21), where the prompt travels on the wire rather
    /// than being typed.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_prompt(
        &mut self,
        agent_kind: AgentKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        shell_override: Option<&str>,
        hooks: Option<&crate::hooks::HookInjection>,
        prompt: String,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        self.spawn_inner(
            ProcessKind::Agent(agent_kind),
            cwd,
            terminal_font_size_px,
            shell_override,
            hooks,
            vec![prompt],
            window,
            cx,
        )
    }

    /// Records the conversation id a pane is attached to, for a kind that cannot learn one from
    /// hooks - see [`Agent::session_id`]. A no-op for an id that isn't open.
    pub fn set_session_id(&mut self, id: AgentId, session_id: String) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == id) {
            agent.session_id = Some(session_id);
        }
    }

    /// Records the host's own PTY session id a pane attached to - see [`Agent::host_session_id`].
    /// A no-op on [`Agent::host_session_id`] itself for an id that isn't open (the pane could
    /// have been closed in the interval between `SessionSpawn` dispatch and this resolving), but
    /// `Self::pending_exits` is still consulted regardless: if this exact session id already
    /// exited before this call ever ran (real on Linux - see [`Self::pending_exits`]'s own docs),
    /// that exit is applied right now, the same [`Self::forget_host_agent`] path a normally
    /// ordered `event/session-exited` takes.
    pub fn set_host_session_id(&mut self, id: AgentId, session_id: jerry_core::SessionId) {
        if let Some(agent) = self.agents.iter_mut().find(|agent| agent.id == id) {
            agent.host_session_id = Some(session_id.clone());
        }
        if self.pending_exits.remove(&session_id) {
            self.forget_host_agent(id);
        }
    }

    /// The open agent whose process the host knows as `session_id`, if any - the reverse of
    /// [`Self::set_host_session_id`]. What `crate::work_surface::session_exited` resolves an
    /// incoming `event/session-exited` notification's `id` against.
    pub fn agent_for_host_session(&self, session_id: &jerry_core::SessionId) -> Option<AgentId> {
        self.agents
            .iter()
            .find(|agent| agent.host_session_id.as_ref() == Some(session_id))
            .map(|agent| agent.id)
    }

    /// [`Self::pending_exits`]'s own write side: `crate::work_surface::session_exited` calls
    /// this when an `event/session-exited` notification's id matches no agent yet, so
    /// [`Self::set_host_session_id`] can apply it once that agent's id is finally known instead
    /// of losing it - see that field's own docs for the real race this closes.
    pub(crate) fn note_unmatched_session_exit(&mut self, session_id: jerry_core::SessionId) {
        self.pending_exits.insert(session_id);
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
        let hook_extras = hook_extras_for(kind, hooks, id);
        let extras = if leading_args.is_empty() {
            hook_extras
        } else {
            let (mut hook_args, env) = hook_extras.unwrap_or_default();
            leading_args.append(&mut hook_args);
            Some((leading_args, env))
        };
        let mut spec = kind.spec(cwd.clone(), shell_override, extras);
        if let ProcessKind::Agent(agent) = kind {
            if let Some(override_command) = self.binary_overrides.get(&agent) {
                spec.spawn_override = Some(override_command.clone());
            }
        }
        self.spawn_resolved(id, kind, cwd, terminal_font_size_px, spec, window, cx)
    }

    /// Test-only: [`Self::spawn_inner`], but with an explicit [`TerminalSpec`] rather than one
    /// resolved from a [`ProcessKind`] - lets a test exercise agent-table-scoped behaviour
    /// (`crate::work_surface::session_exited`'s own coverage) against a real, genuinely-exiting
    /// process, without requiring a real `claude`/`codex`/`cursor-agent` binary this workspace's
    /// own nextest mitigation deliberately keeps off `PATH`. `kind` is still recorded and still
    /// registered in the host's agent table exactly as [`Self::spawn_inner`] would for a real
    /// [`ProcessKind::Agent`] - the same "downstream code cannot tell the difference" honesty
    /// [`Self::set_kind_for_test`]'s own docs describe.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_with_explicit_command_for_test(
        &mut self,
        kind: ProcessKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        spec: TerminalSpec,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        let id = self.next_id;
        self.next_id += 1;
        self.spawn_resolved(id, kind, cwd, terminal_font_size_px, spec, window, cx)
    }

    /// The shared tail of [`Self::spawn_inner`] and (in tests)
    /// [`Self::spawn_with_explicit_command_for_test`]: everything that only needs an already-
    /// allocated `id` and an already-resolved `spec`, regardless of where either came from -
    /// table registration, the pane and its tab, and the `SessionSpawn` dispatch/attach task.
    #[allow(clippy::too_many_arguments)]
    fn spawn_resolved(
        &mut self,
        id: AgentId,
        kind: ProcessKind,
        cwd: PathBuf,
        terminal_font_size_px: f32,
        spec: TerminalSpec,
        window: &mut Window,
        cx: &mut Context<AdeApp>,
    ) -> AgentId {
        if let Some(table) = &self.host_agents {
            if let ProcessKind::Agent(agent_kind) = kind {
                table.register(
                    host_agent_id(id),
                    cwd.clone(),
                    agent_kind.label().to_owned(),
                );
            }
        }

        let pane = cx.new(|cx| TerminalPane::new(spec.clone(), terminal_font_size_px, cx));
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

        let spawn_pane = pane.downgrade();
        let spawn_cwd = cwd.clone();
        self.agents.push(Agent {
            id,
            kind,
            cwd: cwd.clone(),
            pane,
            spawned_at: Instant::now(),
            spawned_at_unix: unix_now(),
            session_id: None,
            host_session_id: None,
            _pane_subscription: pane_subscription,
        });
        self.active = Some(id);
        self.active_by_cwd.insert(cwd, id);

        // Dispatches `SessionSpawn` through the host and attaches this pane to the real
        // `SessionHandle` once it resolves (`docs/architecture/decisions.md` §23) - the tab
        // itself appears immediately, above; the real process attaches asynchronously, exactly
        // as it always has, just through the host rather than a direct `jerry_pty::spawn` call.
        // A `ui`-tier test's stub takes over here, never in `spec.program`/`args` themselves -
        // see `TerminalSpec::spawn_override`.
        let (program, args) = spec
            .spawn_override
            .clone()
            .unwrap_or_else(|| (spec.program.clone(), spec.args.clone()));
        let spawn_task = cx.spawn(async move |this, cx| {
            let dispatch = this.update(cx, |this, cx| {
                this.dispatch(
                    spawn_cwd.clone(),
                    Request::Command(AppCommand::SessionSpawn(SessionSpawn {
                        program: program.clone(),
                        args: args.clone(),
                        env: spec.env.clone(),
                        rows: TERMINAL_ROWS,
                        cols: TERMINAL_COLS,
                        // Still `None` here: this call site still registers an agent's identity
                        // separately, through the in-process `AgentTable` shortcut just above
                        // (`Self::host_agents`) - moving that association onto this field instead,
                        // and retiring the shortcut, is `Hosts`' own per-repository follow-up
                        // (`docs/architecture/decisions.md` §24), since a real, separate
                        // `jerry-host` process has no shortcut left to reach into.
                        agent: None,
                    })),
                    cx,
                )
            });
            let Ok(dispatch) = dispatch else {
                return; // the app itself was dropped before this could even be asked
            };
            let outcome = match dispatch.await {
                Ok(Report::Ok { outcome }) => outcome,
                Ok(other) => {
                    let _ = spawn_pane.update(cx, |pane, cx| {
                        pane.mark_spawn_failed(
                            format!("failed to start {}: {other:?}", spec.program.display()),
                            cx,
                        )
                    });
                    return;
                }
                Err(error) => {
                    let _ = spawn_pane.update(cx, |pane, cx| {
                        pane.mark_spawn_failed(
                            format!(
                                "failed to start {}: {}",
                                spec.program.display(),
                                error.message
                            ),
                            cx,
                        )
                    });
                    return;
                }
            };
            let Some(session_id) = outcome
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(jerry_core::SessionId::from)
            else {
                let _ = spawn_pane.update(cx, |pane, cx| {
                    pane.mark_spawn_failed(
                        "internal error: the host's session-spawn outcome had no id".to_string(),
                        cx,
                    )
                });
                return;
            };
            let attached = this.update(cx, |this, _cx| {
                let handle = this
                    .sessions_for(&spawn_cwd)
                    .and_then(|sessions| sessions.handle_for(&session_id));
                this.agents.set_host_session_id(id, session_id.clone());
                (handle, this.host_client_for(&spawn_cwd))
            });
            let Ok((Some(handle), client)) = attached else {
                let _ = spawn_pane.update(cx, |pane, cx| {
                    pane.mark_spawn_failed(
                        "internal error: the session host could not hand back this session's \
                         adapter"
                            .to_string(),
                        cx,
                    )
                });
                return;
            };
            let attach_outcome = spawn_pane.update(cx, |pane, cx| {
                pane.attach_session(handle.clone(), client, cx)
            });
            if attach_outcome.is_err() {
                // The pane entity itself is already gone - not just doomed, which `TerminalPane::
                // attach_session` already handles on its own by leaving the session for whichever
                // caller is already polling `TerminalPane::take_session_for_teardown` to pick up
                // and shut down. Nothing will ever reach this session through a pane again, so
                // this task must kill it itself rather than silently drop the handle: unlike the
                // old direct `jerry_pty::spawn`, `SessionManager` keeps a session's real process
                // alive independent of any pane, so dropping the handle alone leaks it (GitHub
                // issue #530's own regression).
                cx.background_executor()
                    .spawn(async move {
                        if let Err(err) = handle.shutdown() {
                            log::warn!(
                                "failed to shut down a session whose pane was already gone by \
                                 attach time: {err}"
                            );
                        }
                    })
                    .detach();
            }
        });
        self._spawn_tasks.push(spawn_task);
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

    /// Test-only (GitHub issue #530): makes every future spawn of `kind` really exec `program`
    /// with `args` in place of `AgentKind::binary_name()` and whatever arguments the real spawn
    /// path would have computed (hook injection, `--resume`, ...). The recorded
    /// [`TerminalSpec::program`]/[`TerminalSpec::args`] ([`ProcessKind::spec`]'s own output)
    /// are untouched - only [`TerminalSpec::spawn_override`] carries this, so a test asserting
    /// on the spec via `TerminalPane::spec_for_test` still sees the kind's real binary name and
    /// real arguments. A `ui`-tier test must never launch a real agent CLI; the real args are
    /// meaningless to a stub and, on Windows, actively break one (`more.com` treats an
    /// unrecognized flag as a filename to open and exits immediately rather than blocking -
    /// verified against a real `more.com`), which is why the whole invocation is replaced
    /// rather than just the program name.
    #[cfg(test)]
    pub(crate) fn override_binary(&mut self, kind: AgentKind, program: PathBuf, args: Vec<String>) {
        self.binary_overrides.insert(kind, (program, args));
    }

    /// Test-only: undoes [`Self::override_binary`] for `kind`, for the rare test that genuinely
    /// needs a real agent CLI on the spawn path (the `external`-tier hook integration test).
    #[cfg(test)]
    pub(crate) fn clear_binary_override(&mut self, kind: AgentKind) {
        self.binary_overrides.remove(&kind);
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
    /// tab too - a no-op if `id` doesn't name a currently open agent.
    pub fn set_active(&mut self, id: AgentId) {
        if let Some(agent) = self.agents.iter().find(|agent| agent.id == id) {
            self.active = Some(id);
            self.active_by_cwd.insert(agent.cwd.clone(), id);
        }
    }

    /// Makes `cwd`'s own last-active tab (see [`Self::active_by_cwd`]) the globally active
    /// agent - or, if `cwd` has never had one recorded (a worktree just visited for the
    /// first time this window), its first open agent in creation order. `None` if `cwd`
    /// currently has no open agents at all.
    pub fn activate_for_worktree(&mut self, cwd: &Path) {
        let id = self.primary_for_cwd(cwd).map(|agent| agent.id);
        self.active = id;
        if let Some(id) = id {
            self.active_by_cwd.insert(cwd.to_path_buf(), id);
        }
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
    pub fn clear_active(&mut self) {
        self.active = None;
    }

    /// Test-only: the host agent table's own ids right now, straight from
    /// `jerry_host::AgentTable::list` - the same underlying state `AgentsQuery` answers from
    /// (`crate::work_surface::session_exited`'s own coverage), without a full dispatch round
    /// trip. Empty with no host table yet.
    #[cfg(test)]
    pub(crate) fn host_agent_ids_for_test(&self) -> Vec<jerry_core::AgentId> {
        self.host_agents
            .as_ref()
            .map(|table| table.list().into_iter().map(|(id, _)| id).collect())
            .unwrap_or_default()
    }

    /// Forgets `id`'s entry in the host's agent table (`Self::host_agents`) without touching its
    /// tab or process - for `crate::work_surface::session_exited`'s reaction to an
    /// `event/session-exited` notification, which must stop `AgentsQuery` listing an agent whose
    /// process has genuinely ended even while [`Self::close`]'s "an unclean exit keeps its tab
    /// open" policy leaves the tab itself in place. A no-op with no host table yet.
    pub fn forget_host_agent(&self, id: AgentId) {
        if let Some(table) = &self.host_agents {
            table.forget(&host_agent_id(id));
        }
    }

    /// Removes `session_id`'s entry from the host's session table entirely
    /// (`jerry_host::AgentTable::forget_session`) - the real id `SessionSpawn` minted, distinct
    /// from [`Self::forget_host_agent`]'s synthetic `agent:<id>` key. Without a caller ever doing
    /// this, every session this host ever spawned stayed in the table (dead `PtySession` record
    /// and all) until the host itself exited, and `jerry sessions` listed every one of them
    /// forever (GitHub issue #530's own follow-up). A no-op with no host table yet.
    pub fn forget_session(&self, session_id: &jerry_core::SessionId) {
        if let Some(table) = &self.host_agents {
            table.forget_session(session_id);
        }
    }

    /// Moves keyboard focus onto the currently active agent's terminal pane, if there is
    /// one - a no-op when [`Self::active`] is `None`. See [`Self::spawn`]'s docs for why
    /// callers, not this method, decide whether it's safe to call.
    pub fn focus_active(&self, window: &mut Window, cx: &mut App) {
        if let Some(agent) = self.active() {
            window.focus(&agent.pane.focus_handle(cx), cx);
        }
    }

    /// Closes a tab: dooms its pane's session (the same doom-poll-shutdown sequence
    /// [`crate::worktree_history::flow::AdeApp::execute_discard_worktree_path`] uses, via
    /// [`collect_doomed_sessions`]) before dropping the `Entity<TerminalPane>`, so closing a tab
    /// never just hides it while its process leaks - including one whose `SessionSpawn` dispatch
    /// was still in flight at the moment of the close (GitHub issue #530's own follow-up: without
    /// this, that session would attach after the fact and run unsupervised forever, and its dead
    /// entry would never leave the host's session table either - see [`Self::forget_host_agent`]
    /// and `jerry_host::AgentTable::forget_session`).
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
        let pane = self.agents[index].pane.clone();
        if let Some(table) = &self.host_agents {
            table.forget(&host_agent_id(id));
        }
        let host_agents = self.host_agents.clone();
        let task = cx.spawn(async move |this, cx| {
            let sessions = collect_doomed_sessions(vec![pane], &this, cx).await;
            let session_ids: Vec<jerry_core::SessionId> = sessions
                .iter()
                .map(|session| session.id().clone())
                .collect();
            cx.background_executor()
                .spawn(async move {
                    for session in sessions {
                        if let Err(err) = session.shutdown() {
                            log::warn!("failed to shut down a closed tab's session: {err}");
                        }
                    }
                })
                .await;
            if let Some(table) = host_agents {
                for session_id in session_ids {
                    table.forget_session(&session_id);
                }
            }
        });
        self._close_tasks.push(task);
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
    }
}

/// Marks every pane in `panes` doomed and collects its session for teardown - the real,
/// already-attached ones immediately (`TerminalPane::take_session_for_teardown`), and any still
/// in flight `SessionSpawn` dispatch by polling `TerminalPane::teardown_still_pending` until it
/// settles, so a spawn that resolves after the caller stopped waiting for it is never left
/// running unsupervised (GitHub issue #470/#530's own regressions). Shared by [`Agents::close`]
/// and `crate::worktree_history::flow::AdeApp::execute_discard_worktree_path`, whose own callers
/// shut each returned session down (and forget it, where that applies) themselves, off the UI
/// thread - this only collects them.
///
/// A real, scheduled no-op is what each poll attempt yields on, never
/// `cx.background_executor().timer()`: GPUI's test scheduler only ever advances its simulated
/// clock against an explicit `advance_clock`, so a `timer()` would never resolve under it; a real
/// background task is what `run_until_parked` already blocks on.
pub(crate) async fn collect_doomed_sessions(
    panes: Vec<Entity<TerminalPane>>,
    this: &WeakEntity<AdeApp>,
    cx: &mut AsyncApp,
) -> Vec<Arc<dyn SessionAdapter>> {
    let mut doomed_sessions = Vec::with_capacity(panes.len());
    let mut pending_panes = Vec::new();
    for pane in panes {
        let outcome = this.update(cx, |_this, cx| {
            pane.update(cx, |pane, cx| pane.take_session_for_teardown(cx))
        });
        match outcome {
            Ok(Some(session)) => doomed_sessions.push(session),
            Ok(None) => pending_panes.push(pane),
            Err(_) => break, // the app itself was dropped
        }
    }

    const POLL_ATTEMPTS: u32 = 10_000;
    for pane in pending_panes {
        for attempt in 0..POLL_ATTEMPTS {
            let resolved = this.update(cx, |_this, cx| {
                pane.update(cx, |pane, cx| {
                    (
                        pane.take_session_for_teardown(cx),
                        pane.teardown_still_pending(),
                    )
                })
            });
            let Ok((session, still_pending)) = resolved else {
                break; // the app itself was dropped
            };
            if let Some(session) = session {
                doomed_sessions.push(session);
                break;
            }
            if !still_pending {
                break; // resolved to a spawn failure - nothing left to shut down
            }
            if attempt + 1 == POLL_ATTEMPTS {
                log::warn!(
                    "a doomed pane's SessionSpawn never settled before its owner stopped \
                     waiting for it - it may still be starting after the fact"
                );
            }
            cx.background_executor().spawn(async {}).await;
        }
    }
    doomed_sessions
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
            vec![("JERRY_HOST_SOCKET".to_owned(), "/tmp/jerry.sock".to_owned())],
        ));
        let shell = ProcessKind::Shell.spec(PathBuf::from("/tmp"), None, extras);
        assert!(shell.args.is_empty(), "a shell must get no injected args");
        assert!(
            shell.env.is_empty(),
            "a shell must never see the hook environment"
        );
    }

    #[test]
    fn an_agent_given_a_hook_injection_really_spawns_with_it() {
        // The other half: the injection must actually reach the spawn, or the whole side-channel
        // silently does nothing.
        let extras = Some((
            vec!["--settings".to_owned(), "/tmp/jerry.json".to_owned()],
            vec![("JERRY_HOST_SOCKET".to_owned(), "/tmp/jerry.sock".to_owned())],
        ));
        let spec = ProcessKind::claude().spec(PathBuf::from("/tmp"), None, extras);
        assert_eq!(spec.program, PathBuf::from("claude"));
        assert_eq!(spec.args, vec!["--settings", "/tmp/jerry.json"]);
        assert!(spec
            .env
            .contains(&("JERRY_HOST_SOCKET".to_owned(), "/tmp/jerry.sock".to_owned())));
        // Every real agent spawn also gets `jerry`'s own directory prepended to PATH (this test
        // binary really does have one, via the `jerry` bin target's `CARGO_BIN_EXE_jerry` sibling
        // - see `with_jerry_on_path`'s own tests for the injectable, deterministic form of this).
        assert_eq!(
            spec.env.iter().filter(|(key, _)| key == "PATH").count(),
            1,
            "{:?}",
            spec.env
        );
    }

    #[test]
    fn cursor_gets_env_only_hook_extras_claude_keeps_its_settings_flag_and_codex_gets_neither() {
        // GitHub issue #479's real gate, exercised against a real `HookInjection` (not a
        // hand-built fake tuple) - `cursor-agent` has no `--settings`-equivalent flag, so it must
        // never receive one, while still getting the same env pair Claude does.
        let injection = crate::hooks::HookInjection::for_test(
            PathBuf::from("/tmp/jerry-hook-settings.json"),
            PathBuf::from("/tmp/jerry.sock"),
        );

        let (claude_args, claude_env) = hook_extras_for(ProcessKind::claude(), Some(&injection), 1)
            .expect("claude must get extras");
        assert!(
            claude_args.iter().any(|arg| arg == "--settings"),
            "claude keeps its --settings flag: {claude_args:?}"
        );
        assert!(
            claude_args.iter().any(|arg| arg == "--plugin-dir"),
            "claude also gets the jerry skill plugin: {claude_args:?}"
        );
        assert!(claude_env.iter().any(|(key, _)| key == "JERRY_HOST_SOCKET"));
        assert!(claude_env.iter().any(|(key, _)| key == "JERRY_AGENT_ID"));

        let (cursor_args, cursor_env) = hook_extras_for(ProcessKind::cursor(), Some(&injection), 1)
            .expect("cursor must get extras");
        assert!(
            cursor_args.is_empty(),
            "cursor-agent has no --settings-equivalent flag: {cursor_args:?}"
        );
        assert!(cursor_env.iter().any(|(key, _)| key == "JERRY_HOST_SOCKET"));

        assert!(
            hook_extras_for(ProcessKind::codex(), Some(&injection), 1).is_none(),
            "codex has no hook wiring at all yet"
        );
        assert!(
            hook_extras_for(ProcessKind::cursor(), None, 1).is_none(),
            "no injection offered (the setting is off) means no extras, not a bare env_only"
        );
    }

    #[test]
    fn a_located_jerry_binarys_directory_is_prepended_to_the_spawned_agents_path() {
        let dir = if cfg!(windows) {
            PathBuf::from(r"C:\jerry-bin")
        } else {
            PathBuf::from("/opt/jerry-bin")
        };
        let jerry_binary = dir.join(if cfg!(windows) { "jerry.exe" } else { "jerry" });
        let env = with_jerry_on_path(Vec::new(), || Some(jerry_binary.clone()));
        let path = env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.clone())
            .expect("a PATH entry was added");
        let first_entry = std::env::split_paths(&path)
            .next()
            .expect("at least one entry");
        assert_eq!(first_entry, dir, "{path}");
    }

    #[test]
    fn a_located_jerry_binary_is_prepended_ahead_of_the_rest_of_the_inherited_path() {
        let dir = PathBuf::from("jerry-bin-dir");
        let joined =
            prepend_dir_to_path(&dir, Some(std::ffi::OsStr::new("existing-dir"))).expect("joins");
        let entries: Vec<_> = std::env::split_paths(&joined).collect();
        assert_eq!(entries, vec![dir, PathBuf::from("existing-dir")]);
    }

    #[test]
    fn a_locate_miss_leaves_the_env_untouched_rather_than_blocking_the_spawn() {
        let env = with_jerry_on_path(vec![("EXISTING".to_owned(), "1".to_owned())], || None);
        assert_eq!(env, vec![("EXISTING".to_owned(), "1".to_owned())]);
    }

    #[test]
    fn every_agent_spec_converts_to_its_matching_agent_kind() {
        assert_eq!(
            AgentKind::from(jerry_core::AgentSpec::Claude),
            AgentKind::Claude
        );
        assert_eq!(
            AgentKind::from(jerry_core::AgentSpec::Codex),
            AgentKind::Codex
        );
        assert_eq!(
            AgentKind::from(jerry_core::AgentSpec::Cursor),
            AgentKind::Cursor
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
            // PATH injection (`with_jerry_on_path`) is independent of hook injection - it is
            // the one thing every real agent spawn gets regardless - so `env` may legitimately
            // carry a `PATH` entry here and nothing else.
            assert!(
                spec.env.iter().all(|(key, _)| key == "PATH"),
                "{agent:?} with no hook injection gets no extra environment beyond PATH: {:?}",
                spec.env
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
                let hook_injection = this.hook_injection_for(ProcessKind::Agent(agent_kind), cx);
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
    match jerry_pty::capture_first_line(binary, &["create-chat"], cwd, CHAT_ID_MINT_TIMEOUT) {
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

/// The identity the host knows an agent by: the same text `JERRY_AGENT_ID` carries.
fn host_agent_id(id: AgentId) -> jerry_core::AgentId {
    jerry_core::AgentId(id.to_string())
}

/// `Agents::pending_exits`'s own coverage: a real, in-process `jerry_host::Host` (no `gpui`, no
/// real spawned process needed) proves the exit is applied the moment the id it was waiting for
/// arrives, deterministically - the real-process ordering
/// `session_exited::tests::a_real_agents_exit_is_forgotten_from_the_agent_table_once_its_event_
/// arrives` covers is inherently racy to reproduce on demand (that is the whole bug), so this
/// drives the same two calls directly instead of hoping a real `sh -c exit` finishes fast enough.
#[cfg(test)]
mod pending_exit_tests {
    use super::{host_agent_id, Agents};
    use std::path::PathBuf;

    #[test]
    fn a_session_exit_recorded_before_the_id_is_known_is_applied_the_moment_it_is() {
        let host = jerry_host::Host::start().expect("host");
        let mut agents = Agents::new();
        agents.attach_host(host.agents());

        let agent_id = 1;
        host.agents().register(
            host_agent_id(agent_id),
            PathBuf::from("/repo"),
            "Claude".into(),
        );
        let session_id = jerry_core::SessionId::from("session-1");

        // The exit arrives first - before anything has told `Agents` this session id belongs to
        // `agent_id` at all, exactly the Linux race this closes.
        agents.note_unmatched_session_exit(session_id.clone());
        assert!(
            host.agents()
                .list()
                .iter()
                .any(|(id, _)| *id == host_agent_id(agent_id)),
            "sanity check: still registered - nothing has applied the exit yet"
        );

        // The spawn response resolves after the fact.
        agents.set_host_session_id(agent_id, session_id);

        assert!(
            !host
                .agents()
                .list()
                .iter()
                .any(|(id, _)| *id == host_agent_id(agent_id)),
            "a session exit recorded before its agent id was known must be applied the moment \
             set_host_session_id learns it"
        );
        host.shutdown_and_join();
    }

    /// The ordinary case must keep working unchanged: an id `set_host_session_id` already knows
    /// about, with no pending exit recorded for it, is left alone.
    #[test]
    fn a_session_with_no_pending_exit_is_left_registered() {
        let host = jerry_host::Host::start().expect("host");
        let mut agents = Agents::new();
        agents.attach_host(host.agents());

        let agent_id = 1;
        host.agents().register(
            host_agent_id(agent_id),
            PathBuf::from("/repo"),
            "Claude".into(),
        );
        let session_id = jerry_core::SessionId::from("session-1");

        agents.set_host_session_id(agent_id, session_id);

        assert!(
            host.agents()
                .list()
                .iter()
                .any(|(id, _)| *id == host_agent_id(agent_id)),
            "no exit was ever recorded - the agent must stay registered"
        );
        host.shutdown_and_join();
    }
}
