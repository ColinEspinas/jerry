//! Binary entry point: resolves the target repository path from an optional CLI argument.
//!
//! GitHub issue #90: this deliberately no longer falls back to `env::current_dir()` - a fresh
//! launch with no CLI argument now hands `app::run` a real `None` instead of silently forcing
//! whatever directory the process happened to be started from open as a "repo". `app::run` (and,
//! beneath it, `root::AdeApp::new_with_settings`) resolves what to do with `None` itself: reopen
//! the last-remembered folder if one was persisted, or a genuinely empty window if not - see
//! those functions' own docs.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let repo_path = env::args_os().nth(1).map(PathBuf::from);

    app::run(repo_path);
    ExitCode::SUCCESS
}
