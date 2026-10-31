//! A minimal, real stand-in for `jerry git-sequence-editor`/`jerry git-editor`
//! (`crates/jerry-cli/src/lib.rs`'s own hidden subcommands), built only so `jerry-git`'s own
//! `rebase` tests can hand `start_interactive_rebase` a genuinely separate, executable process -
//! see `crates/jerry-git/Cargo.toml`'s `[[bin]]` entry for why this lives here rather than as a
//! dev-dependency on `jerry-cli`. Never shipped: this package's own binary, test-only in practice.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let subcommand = args.next().unwrap_or_default();
    let target = args.next().map(PathBuf::from);
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("jerry-git-test-editor: could not read cwd: {error}");
            return ExitCode::FAILURE;
        }
    };

    let Some(target) = target else {
        eprintln!("jerry-git-test-editor: missing the file path git appends");
        return ExitCode::FAILURE;
    };

    match subcommand.to_str() {
        Some("git-sequence-editor") => {
            match jerry_git::rebase::run_sequence_editor(&cwd, &target) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("jerry-git-test-editor: git-sequence-editor failed: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("git-editor") => match jerry_git::rebase::run_editor(&cwd, &target) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                eprintln!("jerry-git-test-editor: git-editor failed: {error}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("jerry-git-test-editor: unknown subcommand {other:?}");
            ExitCode::FAILURE
        }
    }
}
