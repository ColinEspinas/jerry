//! The command line as clap sees it. One subcommand per Request the CLI exposes; the rule for
//! what belongs here is "what git alone cannot answer", plus `status`.

use clap::{Parser, Subcommand};
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
}
