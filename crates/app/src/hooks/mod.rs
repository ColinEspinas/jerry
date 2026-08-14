//! A real, structural status side-channel for Claude Code agents (GitHub issue #239, phase 2).
//!
//! Phase 1 made Jerry's rail read what an agent CLI *rendered* - a title glyph, an OSC ping - as
//! a refinement on top of guessing from pty silence. Both are inferences about a process from the
//! outside. This is the other thing: Claude Code's own hook system, a documented side-channel
//! that fires a command at named lifecycle events and hands it a JSON payload describing what
//! just happened. `Stop` fires when a turn ends. `PermissionRequest` fires when the agent is
//! genuinely blocked on a human. `PreToolUse` carries the actual tool and the actual argument.
//! None of that is inferable from silence, and all of it is exactly what a "who needs me" rail
//! wants to know.
//!
//! ## How the pieces fit
//!
//! ```text
//!   AdeApp::new_agent(Agent(Claude))
//!     ├── AdeApp::hook_injection_for  ── first Claude agent only ──▶ HookRuntime::start()
//!     │                                    ├── HookListener  (127.0.0.1:<ephemeral>, token)  [server]
//!     │                                    └── HookFiles     (forwarder + --settings JSON)   [settings_file]
//!     └── spawn `claude --settings <HookFiles::settings_path()>`
//!           with JERRY_HOOK_PORT / JERRY_HOOK_TOKEN / JERRY_AGENT_ID in its environment
//!
//!   claude fires a hook
//!     └── forwarder script (dumb; guards on the env vars; always exits 0)
//!           └── POST /hook?event=<name>&agent=<id>  ──▶  HookListener
//!                 └── hooks::event::parse  ──▶  HookInbox (latest fact per agent)
//!
//!   rail render
//!     └── AdeApp::agent_status ──▶ rail::status::derive_status(.., HookSignal, ..)
//!     └── AdeApp::build_agent_rows ──▶ AgentRow::activity
//!                                       └── AgentStatusState (agent-status.toml)  [store]
//!
//!   agent closes / app restarts, later reopened (GitHub issue #227)
//!     └── AdeApp::build_worktree_rows ──▶ history::past_agents_for_worktree(state, wt, live_keys)
//!                                          └── WorktreeRow::history  ──▶  rail "History" rows
//!     └── click Resume ──▶ AdeApp::resume_past_agent
//!           ├── real session_id captured  ──▶ Agents::spawn_resume (`claude --resume <id>`)
//!           └── no session_id (Codex, or predates this field) ──▶ Agents::spawn (fresh agent)
//! ```
//!
//! ## Claude Code only, and per-launch only
//!
//! Hooks are a Claude Code feature, so this is installed for
//! [`crate::work_surface::agents::AgentKind::Claude`] spawns and nothing else. Codex agents and
//! shells are completely untouched and keep exactly the Phase 1 behaviour - which is also the
//! fallback for a Claude agent whose hooks haven't fired yet, or have gone stale.
//!
//! Injection is strictly per-launch: a generated `--settings` file passed on the command line of
//! the specific `claude` process Jerry spawned. Jerry never writes to `~/.claude/settings.json`,
//! `.claude/settings.json`, or any other file Claude Code reads on its own, so a `claude` the
//! user starts from their own terminal behaves exactly as if Jerry were not installed. And
//! because Claude Code merges hook arrays across every settings layer (verified against a real
//! binary - see [`settings_file`]'s module docs), Jerry adding its hooks never disables the
//! user's own.

pub mod event;
pub mod flow;
pub mod history;
pub mod server;
pub mod settings_file;
pub mod store;

#[cfg(test)]
mod integration_tests;

use std::path::{Path, PathBuf};

use crate::work_surface::agents::AgentId;

/// Everything one Jerry launch needs to receive hooks: the live listener and the generated files
/// on disk. Dropping it stops the listener and removes the files.
pub struct HookRuntime {
    listener: server::HookListener,
    files: settings_file::HookFiles,
}

impl HookRuntime {
    /// Starts the listener and writes this instance's forwarder script and settings file into
    /// `parent` (the OS temp directory in production).
    ///
    /// Called lazily, on the first Claude agent an `AdeApp` spawns - see
    /// [`crate::root::AdeApp::hook_injection_for`] for why, and for the guarantee that there is
    /// still only ever one of these per `AdeApp`.
    ///
    /// Returns `None` rather than an error if hook support can't be brought up - a loopback that
    /// won't bind, an unwritable temp directory, or a platform with no forwarder written for it
    /// ([`settings_file::is_supported`]). Every one of those is a real state, and none of them
    /// should stop Jerry starting: without a runtime, every agent simply falls back to the Phase 1
    /// title/OSC and quiescence signals, which is exactly the behaviour that shipped before this
    /// phase.
    ///
    /// The two machine-level failures - the port and the temp directory - apply identically on
    /// every supported platform. The platform check is *not* the Unix-only gate it used to be:
    /// macOS, Linux and native Windows all install real hooks, each through its own forwarder and
    /// its own pinned shell (see [`settings_file`]'s module docs). It is only the genuinely
    /// unwritten-for targets that fall through it now.
    pub fn start(parent: &Path) -> Option<HookRuntime> {
        if !settings_file::is_supported() {
            log::info!(
                "agent hooks are not supported on this platform - agent status will use the \
                 terminal-title and quiescence signals only"
            );
            return None;
        }
        let listener = match server::HookListener::start() {
            Ok(listener) => listener,
            Err(err) => {
                log::warn!("could not start the agent hook listener ({err}) - falling back to the terminal-title and quiescence signals");
                return None;
            }
        };
        let files = match settings_file::HookFiles::write_in(parent) {
            Ok(files) => files,
            Err(err) => {
                log::warn!("could not write the agent hook settings ({err}) - falling back to the terminal-title and quiescence signals");
                return None;
            }
        };
        log::info!("agent hook listener ready on 127.0.0.1:{}", listener.port());
        Some(HookRuntime { listener, files })
    }

    /// An owned handle carrying everything a spawn needs, detached from `self`.
    ///
    /// Owned rather than a borrow because of a real constraint at the call site:
    /// `crate::work_surface::render::AdeApp::new_agent` calls `self.agents.spawn(..)`, which
    /// takes `&mut self.agents`, and would therefore not also be able to hold a `&self.hooks`
    /// borrow across the same call. Cloning three small values once per spawn is cheaper than
    /// restructuring `AdeApp`'s field ownership around it.
    pub fn injection(&self) -> HookInjection {
        HookInjection {
            settings_path: self.files.settings_path().to_path_buf(),
            port: self.listener.port(),
            token: self.listener.token().to_owned(),
        }
    }

    /// This agent's current hook fact, for [`crate::rail::status::derive_status`].
    pub fn signal_for(&self, id: AgentId) -> crate::rail::status::HookSignal {
        self.listener.signal_for(id)
    }

    /// This agent's current hook-derived `(activity, question)` text, for its rail row.
    pub fn text_for(&self, id: AgentId) -> (Option<String>, Option<String>) {
        self.listener.text_for(id)
    }

    /// This agent's real Claude Code `session_id` (GitHub issue #227), if its hooks have ever
    /// reported one - see [`server::HookListener::session_id_for`] for why this deliberately
    /// isn't gated by staleness the way [`Self::text_for`] is.
    pub fn session_id_for(&self, id: AgentId) -> Option<String> {
        self.listener.session_id_for(id)
    }

    /// Drops an agent's recorded facts, so a future [`AgentId`] can't inherit them.
    pub fn forget(&self, id: AgentId) {
        self.listener.forget(id);
    }
}

/// An owned, spawn-time snapshot of a [`HookRuntime`] - see [`HookRuntime::injection`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookInjection {
    /// Kept as a real [`PathBuf`], not a `String`.
    ///
    /// It used to be built with `to_string_lossy().into_owned()`, which is silently wrong on a
    /// non-UTF-8 `TMPDIR`: the invalid bytes become U+FFFD, and `claude --settings` is then handed
    /// a path that does not exist, with no error anywhere and hooks simply never firing. A
    /// `PathBuf` survives any byte sequence the OS accepts, and the one place a conversion is
    /// genuinely unavoidable ([`Self::spawn_extras`], because
    /// `crate::terminal::pane::TerminalSpec::args` is `Vec<String>`) now fails loudly instead.
    settings_path: PathBuf,
    port: u16,
    token: String,
}

impl HookInjection {
    /// The extra CLI arguments and environment a freshly spawned `claude` needs so its hooks
    /// reach this Jerry - `(args, env)`, ready for `crate::terminal::pane::TerminalSpec`.
    ///
    /// The agent's identity travels in the environment rather than in the settings file, which is
    /// exactly why one generated file serves every agent this launch spawns: the file is
    /// identical for all of them, and `JERRY_AGENT_ID` is what tells the listener which row an
    /// event belongs to.
    /// `None` if the settings path is not representable as UTF-8, which is the only place the
    /// `PathBuf` -> `String` conversion cannot be avoided (`TerminalSpec::args` is `Vec<String>`).
    /// Refusing loudly here means such an agent spawns with no hooks and a real log line, rather
    /// than with a mangled `--settings` path that silently points at nothing.
    pub fn spawn_extras(&self, id: AgentId) -> Option<crate::work_surface::agents::SpawnExtras> {
        let Some(settings_path) = self.settings_path.to_str() else {
            log::warn!(
                "the generated hook settings path ({}) is not valid UTF-8, so it cannot be passed \
                 as a command-line argument - this agent will fall back to the terminal-title and \
                 quiescence signals",
                self.settings_path.display()
            );
            return None;
        };
        let args = vec!["--settings".to_owned(), settings_path.to_owned()];
        let env = vec![
            (settings_file::PORT_ENV.to_owned(), self.port.to_string()),
            (settings_file::TOKEN_ENV.to_owned(), self.token.clone()),
            (settings_file::AGENT_ENV.to_owned(), id.to_string()),
        ];
        Some((args, env))
    }
}
