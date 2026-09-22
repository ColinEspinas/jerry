//! A minimal, real stand-in for the `jerry` binary's hidden `git-sequence-editor`/`git-editor`
//! subcommands (`crates/jerry-cli/src/lib.rs`), built only so `RebaseStart`'s own integration
//! tests (`tests/rebase_commands.rs`) can exercise a real `git rebase -i` end to end - see
//! `crates/jerry-core/Cargo.toml`'s `[[bin]]` entry for why this lives here rather than a
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
            eprintln!("jerry-core-test-jerry: could not read cwd: {error}");
            return ExitCode::FAILURE;
        }
    };

    let Some(target) = target else {
        eprintln!("jerry-core-test-jerry: missing the file path git appends");
        return ExitCode::FAILURE;
    };

    match subcommand.to_str() {
        Some("git-sequence-editor") => {
            match jerry_git::rebase::run_sequence_editor(&cwd, &target) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("jerry-core-test-jerry: git-sequence-editor failed: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("git-editor") => match jerry_git::rebase::run_editor(&cwd, &target) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                eprintln!("jerry-core-test-jerry: git-editor failed: {error}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!("jerry-core-test-jerry: unknown subcommand {other:?}");
            ExitCode::FAILURE
        }
    }
}
