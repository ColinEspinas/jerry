//! A real, structural status side-channel for Claude Code agents (GitHub issue #239, phase 2).

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

    /// This agent's real run-level facts - completed turns and the run's title (GitHub issue
    /// #227). See [`server::HookListener::run_facts_for`].
    pub fn run_facts_for(&self, id: AgentId) -> server::RunFacts {
        self.listener.run_facts_for(id)
    }

    /// Every file write reported since the last call, in arrival order - the signal
    /// `crate::provenance` turns into per-line attribution (GitHub issue #284). Returns
    /// `(edits, dropped)`; see [`server::HookListener::drain_edits`].
    pub fn drain_edits(&self) -> (Vec<server::AgentEdit>, usize) {
        self.listener.drain_edits()
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
    settings_path: PathBuf,
    port: u16,
    token: String,
}

impl HookInjection {
    /// The extra CLI arguments and environment a freshly spawned `claude` needs so its hooks
    /// reach this Jerry - `(args, env)`, ready for `crate::terminal::pane::TerminalSpec`.
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
