//! The command line as clap sees it. One subcommand per Request the CLI exposes; the rule for
//! what belongs here is "what git alone cannot answer", plus `status` and `merge`.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "jerry",
    version,
    about = "Talk to the Jerry that owns this repository, or run what git alone can answer",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Machine-readable output: the Report as JSON on stdout, diagnostics on stderr.
    #[arg(long, global = true)]
    pub json: bool,

    /// The socket of the Jerry to use when several serve this repository.
    #[arg(long, global = true, value_name = "SOCKET")]
    pub instance: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Where am I, who am I, and is a Jerry listening.
    Status,
    /// Merge this worktree's branch into the repository's base branch, through Jerry's own
    /// merge flow: conflicts stay on disk for you to resolve, then `--continue` finishes.
    Merge(MergeArgs),
    /// Create or act on worktrees.
    Wt(WtArgs),
    /// List the agents Jerry is currently supervising.
    Agents,
    /// List every PTY session Jerry is currently tracking, agents and plain terminal tabs alike.
    Sessions,
    /// Wake the human: `Invocability::Allowed`, meant for an agent to call on itself.
    Attention(AttentionArgs),
    /// Write to another live session's stdin over the control plane. `Invocability::Denied` by
    /// default - opened per agent through its own orchestrator grants
    /// (`docs/architecture/decisions.md` §28).
    Send(SendArgs),
    /// Start or stop the Jerry host for this repository directly - for headless use, without
    /// `jerry-app` running (`docs/architecture/decisions.md` §24). Agents never reach this:
    /// `Shutdown`'s own `Invocability::Denied` refuses it, and nothing spawns a host on an
    /// agent's behalf in the first place.
    Host(HostArgs),
    /// Run an MCP server on stdio, exposing every Command/Query as a tool - see
    /// `crates/jerry-cli/skill/SKILL.md`'s "MCP" section.
    Mcp,
    /// Print the `jerry` skill: what these commands do, and when to use them.
    Skill,
    /// Forwards one agent hook event, read from stdin, to the Jerry that spawned this agent.
    /// Jerry's own generated hook entry, not a stable part of the CLI surface - hidden from
    /// `--help` accordingly.
    #[command(hide = true)]
    Hook(HookArgs),
    /// `GIT_SEQUENCE_EDITOR`'s real target during a rebase this Jerry started
    /// (`jerry_git::rebase::start_interactive_rebase`) - copies the prepared todo over git's own
    /// generated one. Never invoked by a human; hidden accordingly.
    #[command(hide = true)]
    GitSequenceEditor(GitSequenceEditorArgs),
    /// `GIT_EDITOR`'s real target during the same rebase - classifies the message file git hands
    /// it (`docs/architecture/decisions.md` §7's three cases) and rewrites or accepts it. Never
    /// invoked by a human; hidden accordingly.
    #[command(hide = true)]
    GitEditor(GitEditorArgs),
}

#[derive(Debug, Args)]
pub struct GitSequenceEditorArgs {
    /// The todo file git generated, to overwrite with the prepared plan.
    pub todo_file: PathBuf,
}

#[derive(Debug, Args)]
pub struct GitEditorArgs {
    /// The commit message file git generated, to classify and possibly rewrite.
    pub message_file: PathBuf,
}

#[derive(Debug, Args)]
pub struct HookArgs {
    /// The agent CLI's own event name, e.g. `PreToolUse`.
    pub event: String,
}

#[derive(Debug, Args)]
pub struct HostArgs {
    #[command(subcommand)]
    pub action: HostAction,
}

#[derive(Debug, Subcommand)]
pub enum HostAction {
    /// Spawn-or-connect: prints the socket of the Jerry host now serving this repository,
    /// spawning one detached if none already does.
    Start,
    /// Ask the Jerry host serving this repository to shut down.
    Stop,
}

#[derive(Debug, Args)]
pub struct WtArgs {
    #[command(subcommand)]
    pub action: WtAction,
}

#[derive(Debug, Subcommand)]
pub enum WtAction {
    /// Create a new worktree on a fresh branch, optionally starting an agent in it.
    New(WtNewArgs),
}

#[derive(Debug, Args)]
pub struct WtNewArgs {
    /// The new branch's name.
    pub branch: String,

    /// The start point for `branch`; defaults to `HEAD`.
    #[arg(long, value_name = "REF")]
    pub from: Option<String>,

    /// Also start this agent CLI in the new worktree: `claude`, `codex`, or `cursor`.
    #[arg(long, value_name = "KIND")]
    pub agent: Option<String>,

    /// Grants the spawned agent orchestrator policy - real control over other agents' sessions
    /// (`jerry send`), gated by `[agents.orchestrator]` grants in Jerry's own settings. Only
    /// meaningful with `--agent`.
    #[arg(long)]
    pub orchestrator: bool,

    /// An initial message for the spawned agent. Only meaningful with `--agent`.
    pub prompt: Option<String>,
}

#[derive(Debug, Args)]
pub struct AttentionArgs {
    /// What to tell the human.
    pub message: String,
}

#[derive(Debug, Args)]
pub struct SendArgs {
    /// The session to write to.
    #[arg(long, value_name = "SESSION_ID")]
    pub to: String,

    /// Leave `text` sitting unentered - don't send the trailing Enter.
    #[arg(long)]
    pub no_submit: bool,

    /// The text to write.
    pub text: String,
}

#[derive(Debug, Args)]
pub struct MergeArgs {
    /// Say whether the merge could run right now, without running it.
    #[arg(long, conflicts_with_all = ["continue_", "abort"])]
    pub dry_run: bool,

    /// After resolving conflicts by hand: stage every fully resolved file and commit the merge.
    #[arg(long = "continue", conflicts_with = "abort")]
    pub continue_: bool,

    /// Give up on the merge in progress and restore the base worktree.
    #[arg(long)]
    pub abort: bool,
}
