//! Binary entry point: resolves the target repository path (an optional CLI argument,
//! defaulting to the current working directory) and hands off to `app::run`.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let repo_path = match env::args_os().nth(1) {
        Some(arg) => PathBuf::from(arg),
        None => match env::current_dir() {
            Ok(dir) => dir,
            Err(err) => {
                eprintln!("failed to determine the current working directory: {err}");
                return ExitCode::FAILURE;
            }
        },
    };

    app::run(repo_path);
    ExitCode::SUCCESS
}
