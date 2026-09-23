//! `jerry-host`: the standalone session-host process for one repository
//! (`docs/architecture/decisions.md` §24). Parses `--repo`/`--registry-dir`, starts a real
//! `Host`, publishes its registry descriptor once listening, then blocks in the lifecycle loop
//! until it should exit - no live sessions and no subscribed client for its linger, or an
//! explicit `shutdown`. Exit code 0 on a clean exit.
//!
//! Never spawned by an agent, and never spawns another `jerry-host` itself - `jerry-app`'s
//! `HostRuntime` and `jerry host start` are the only two spawners (`crate::job_object` is this
//! process's own self-adoption, not the spawn side of that).

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[cfg(windows)]
mod job_object;

use clap::Parser;
use jerry_core::registry::Registry;
use jerry_host::{Host, LifecycleConfig};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "jerry-host",
    version,
    about = "The Jerry session host for one repository"
)]
struct Cli {
    /// The repository this host serves - its common `.git` directory, already resolved by the
    /// spawner (`jerry_core::host_spawn::spawn_or_connect`).
    #[arg(long)]
    repo: PathBuf,
    /// Overrides the registry directory - test-only; production always resolves
    /// `jerry_core::registry::runtime_dir()`.
    #[arg(long)]
    registry_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    env_logger::init();
    // The one place besides `jerry-app`'s own `main` that adopts a fresh kill-on-close job for
    // its own descendants (spike point 3, docs/architecture/decisions.md §14) - this process may
    // itself have arrived here via `CREATE_BREAKAWAY_FROM_JOB`, escaping whatever job spawned it.
    #[cfg(windows)]
    job_object::adopt_this_process();

    let cli = Cli::parse();
    let registry_dir = match cli.registry_dir {
        Some(dir) => dir,
        None => match jerry_core::registry::runtime_dir() {
            Ok(dir) => dir,
            Err(error) => {
                eprintln!("jerry-host: could not resolve the registry directory: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    let registry = match Registry::open(registry_dir) {
        Ok(registry) => registry,
        Err(error) => {
            eprintln!("jerry-host: could not open the registry: {error}");
            return ExitCode::FAILURE;
        }
    };
    let instance = match registry.allocate() {
        Ok(instance) => instance,
        Err(error) => {
            eprintln!("jerry-host: could not reserve a registry entry: {error}");
            return ExitCode::FAILURE;
        }
    };

    let host = match Host::start() {
        Ok(host) => host,
        Err(error) => {
            eprintln!("jerry-host: could not start: {error}");
            return ExitCode::FAILURE;
        }
    };
    // The registry descriptor is published only once this succeeds, so a discoverable entry
    // always has a listener behind it (`Host::listen`'s own contract).
    if let Err(error) = host.listen(&instance.socket) {
        eprintln!(
            "jerry-host: could not listen on {}: {error}",
            instance.socket.display()
        );
        return ExitCode::FAILURE;
    }
    if let Err(error) = registry.publish(&instance, std::slice::from_ref(&cli.repo)) {
        eprintln!("jerry-host: could not publish its descriptor: {error}");
        return ExitCode::FAILURE;
    }
    log::info!(
        "jerry-host: serving {} on {}",
        cli.repo.display(),
        instance.socket.display()
    );

    host.run_lifecycle(LifecycleConfig::default());
    host.shutdown_and_join();
    let _ = registry.remove(&instance);
    ExitCode::SUCCESS
}
