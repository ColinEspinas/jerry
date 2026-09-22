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
