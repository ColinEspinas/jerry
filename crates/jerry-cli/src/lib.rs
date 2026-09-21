//! The `jerry` command. Deliberately shallow: clap builds a `Request`, the transport decides
//! whether a host serves this repository, the `Report` becomes an exit code and output.
//! Everything is a function of its arguments, the environment lookup and the cwd it is handed,
//! so the whole binary is tested without spawning it.

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cli;
pub mod exit;
pub mod transport;

use crate::cli::{Cli, Command};
use crate::transport::{ChooseError, Transport};
use clap::Parser;
use jerry_core::client::{Client, ClientError};
use jerry_core::registry::{runtime_dir_for, Os, Registry};
use jerry_core::wire::rpc_code;
use jerry_core::{
    execute_locally, AgentId, AppQuery, Call, Caller, Ctx, LocalDispatchError, Report, Request,
    RpcError,
};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// The identity Jerry injects into an agent's environment.
pub const AGENT_ENV: &str = "JERRY_AGENT_ID";

/// A host socket to use instead of registry discovery; Jerry injects it into an agent's
/// environment so an agent always reaches the instance that spawned it.
pub const SOCKET_ENV: &str = "JERRY_HOST_SOCKET";

/// How long a connected call may take end to end. A merge can legitimately run for seconds.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Runs one invocation and returns its exit code. `out` receives the result (JSON with
/// `--json`, prose otherwise); `err` receives diagnostics only.
pub fn run(
    args: impl IntoIterator<Item = OsString>,
    env: &dyn Fn(&str) -> Option<OsString>,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) => return usage(error, out, err),
    };
    let caller = match env(AGENT_ENV) {
        Some(id) => Caller::Agent {
            id: AgentId(id.to_string_lossy().into_owned()),
        },
        None => Caller::Human,
    };
    let ctx = match Ctx::from_cwd(cwd, caller) {
        Ok(ctx) => ctx,
        Err(error) => {
            let _ = writeln!(
                err,
                "jerry: {} is not inside a git repository ({error})",
                cwd.display()
            );
            return exit::FAILED;
        }
    };
    let transport = match choose_transport(&cli, env, &ctx, err) {
        Ok(transport) => transport,
        Err(code) => return code,
    };

    let request = match &cli.command {
        Command::Status => Request::Query(AppQuery::Status(Default::default())),
    };
    let outcome = match &transport {
        Transport::Host(descriptor) => connected(
            Client::connect_to(descriptor, CALL_TIMEOUT),
            &ctx,
            request,
            err,
        ),
        Transport::Socket(socket) => {
            connected(Client::connect(socket, CALL_TIMEOUT), &ctx, request, err)
        }
        Transport::Standalone => match execute_locally(&request, &ctx) {
            Ok(report) => Ok(report),
            Err(LocalDispatchError::NeedsHost(method)) => {
                let _ = writeln!(
                    err,
                    "jerry: {method} needs a running Jerry, and none serves this repository"
                );
                Err(exit::NO_INSTANCE)
            }
            Err(LocalDispatchError::Forbidden(method)) => {
                let _ = writeln!(err, "jerry: {method} is not something an agent may ask");
                Err(exit::DENIED)
            }
        },
    };
    let report = match outcome {
        Ok(report) => report,
        Err(code) => return code,
    };

    if cli.json {
        if serde_json::to_writer(&mut *out, &report).is_err() {
            return exit::FAILED;
        }
        let _ = writeln!(out);
    } else {
        render(&cli.command, &report, &transport, out, err);
    }
    exit::for_report(&report)
}

fn usage(error: clap::Error, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    // Help and version are answers, not mistakes: they go to stdout and exit 0.
    if error.use_stderr() {
        let _ = write!(err, "{error}");
        exit::USAGE
    } else {
        let _ = write!(out, "{error}");
        exit::DONE
    }
}

fn choose_transport(
    cli: &Cli,
    env: &dyn Fn(&str) -> Option<OsString>,
    ctx: &Ctx,
    err: &mut dyn Write,
) -> Result<Transport, u8> {
    let registry = match runtime_dir_for(Os::host(), env).and_then(Registry::open) {
        Ok(registry) => Some(registry),
        Err(error) => {
            let _ = writeln!(err, "jerry: no host registry ({error}); running standalone");
            None
        }
    };
    match transport::choose(
        env,
        &ctx.repo_path,
        registry.as_ref(),
        cli.instance.as_deref(),
    ) {
        Ok(transport) => Ok(transport),
        Err(ChooseError::Ambiguous(descriptors)) => {
            let _ = writeln!(
                err,
                "jerry: {} Jerry instances serve this repository; pick one with --instance:",
                descriptors.len()
            );
            for descriptor in descriptors {
                let _ = writeln!(err, "  {}", descriptor.socket.display());
            }
            Err(exit::NO_INSTANCE)
        }
        Err(ChooseError::Registry(error)) => {
            let _ = writeln!(err, "jerry: could not read the host registry: {error}");
            Err(exit::FAILED)
        }
    }
}

fn connected(
    client: Result<Client, ClientError>,
    ctx: &Ctx,
    request: Request,
    err: &mut dyn Write,
) -> Result<Report, u8> {
    let mut client = match client {
        Ok(client) => client,
        Err(error) => {
            let _ = writeln!(
                err,
                "jerry: could not reach the Jerry serving this repository: {error}"
            );
            return Err(exit::NO_INSTANCE);
        }
    };
    let call = match &ctx.caller {
        Caller::Human => Call::human(&ctx.worktree_path, request),
        Caller::Agent { id } => Call::agent(&ctx.worktree_path, id.clone(), request),
    };
    match client.request(&call) {
        Ok(report) => Ok(report),
        Err(ClientError::Rpc(error)) => {
            let _ = writeln!(err, "jerry: {} ({})", error.message, error.code);
            Err(exit::for_rpc_error(&error))
        }
        Err(error) => {
            let _ = writeln!(err, "jerry: {error}");
            Err(exit::FAILED)
        }
    }
}

fn render(
    command: &Command,
    report: &Report,
    transport: &Transport,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    match report {
        Report::Ok { outcome } => match command {
            Command::Status => {
                let field = |name: &str| outcome[name].as_str().unwrap_or("?").to_owned();
                let caller = match &outcome["caller"]["kind"] {
                    kind if kind == "agent" => {
                        format!("agent {}", outcome["caller"]["id"].as_str().unwrap_or("?"))
                    }
                    _ => "human".to_owned(),
                };
                let host = match transport {
                    Transport::Host(descriptor) => {
                        format!("connected ({})", descriptor.socket.display())
                    }
                    Transport::Socket(socket) => format!("connected ({})", socket.display()),
                    Transport::Standalone => "standalone (no Jerry serves this repository)".into(),
                };
                let _ = writeln!(out, "worktree    {}", field("worktree_path"));
                let _ = writeln!(out, "repository  {}", field("repo_path"));
                let _ = writeln!(out, "caller      {caller}");
                let _ = writeln!(out, "host        {host}");
            }
        },
        Report::Denied { code, reason } => {
            let _ = writeln!(err, "jerry: refused: {reason} ({code})");
        }
        Report::Error { error } => {
            let _ = writeln!(err, "jerry: {} ({})", error.message, error.code);
        }
    }
}

/// Maps the errors a host can answer with onto the exit-code contract.
impl exit::ForRpc for RpcError {
    fn exit_code(&self) -> u8 {
        match self.code {
            rpc_code::FORBIDDEN | rpc_code::CONFINED => exit::DENIED,
            rpc_code::NEEDS_HOST | rpc_code::SHUTTING_DOWN | rpc_code::UNSUPPORTED_VERSION => {
                exit::NO_INSTANCE
            }
            rpc_code::METHOD_NOT_FOUND | rpc_code::INVALID_PARAMS | rpc_code::INVALID_REQUEST => {
                exit::USAGE
            }
            _ => exit::FAILED,
        }
    }
}

#[cfg(test)]
mod run_tests {
    use super::{run, AGENT_ENV};
    use jerry_core::registry::{Os, Registry};
    use jerry_core::Report;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use test_support::seed_empty_repo;

    fn env(pairs: &[(&str, &Path)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.as_os_str().to_owned()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    /// The environment variable that decides the runtime directory on this host, so a test's
    /// registry lives in its own temp dir.
    fn runtime_env_key() -> &'static str {
        match Os::host() {
            Os::Windows => "LOCALAPPDATA",
            Os::MacOs => "TMPDIR",
            Os::Unix => "XDG_RUNTIME_DIR",
        }
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        std::iter::once("jerry")
            .chain(list.iter().copied())
            .map(OsString::from)
            .collect()
    }

    fn invoke(
        list: &[&str],
        env: &dyn Fn(&str) -> Option<OsString>,
        cwd: &Path,
    ) -> (u8, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(args(list), env, cwd, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("utf8 stdout"),
            String::from_utf8(err).expect("utf8 stderr"),
        )
    }

    #[test]
    fn status_runs_standalone_when_no_jerry_serves_the_repository() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);

        let (code, out, err) = invoke(&["status", "--json"], &env, repo.path());
        assert_eq!(code, 0, "stderr: {err}");
        let report: Report = serde_json::from_str(out.trim()).expect("a Report on stdout");
        assert!(report.is_ok(), "{report:?}");

        let (code, out, _) = invoke(&["status"], &env, repo.path());
        assert_eq!(code, 0);
        assert!(out.contains("standalone"), "{out}");
    }

    #[test]
    fn status_reaches_a_published_jerry_and_says_so() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);
        let registry_dir =
            jerry_core::registry::runtime_dir_for(Os::host(), &env).expect("runtime dir");
        let registry = Registry::open(registry_dir).expect("registry");
        let instance = registry.allocate().expect("instance");
        let host = jerry_host::Host::start().expect("host");
        host.listen(&instance.socket).expect("listen");
        // The registry is keyed by the common git dir, exactly what the CLI resolves from cwd.
        let common = jerry_core::Ctx::from_cwd(repo.path(), jerry_core::Caller::Human)
            .expect("a repo")
            .repo_path;
        registry.publish(&instance, &[common]).expect("publish");

        let (code, out, err) = invoke(&["status"], &env, repo.path());
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("connected"), "{out}");
        assert!(out.contains("caller      human"), "{out}");

        host.shutdown_and_join();
        registry.remove(&instance).expect("remove");
    }

    #[test]
    fn an_agent_identity_in_the_environment_travels_with_the_call() {
        let repo = seed_empty_repo();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let agent = PathBuf::from("agent-42");
        let env = env(&[(runtime_env_key(), runtime.path()), (AGENT_ENV, &agent)]);

        let (code, out, _) = invoke(&["status"], &env, repo.path());
        assert_eq!(code, 0);
        assert!(out.contains("caller      agent agent-42"), "{out}");
    }

    #[test]
    fn outside_a_repository_is_exit_five_with_a_reason() {
        let nowhere = tempfile::TempDir::new().expect("dir");
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);
        let (code, _, err) = invoke(&["status"], &env, nowhere.path());
        assert_eq!(code, 5);
        assert!(err.contains("not inside a git repository"), "{err}");
    }

    #[test]
    fn a_usage_mistake_is_exit_two_and_help_is_exit_zero() {
        let repo = seed_empty_repo();
        let env = env(&[]);
        let (code, _, err) = invoke(&["frobnicate"], &env, repo.path());
        assert_eq!(code, 2, "{err}");
        let (code, out, _) = invoke(&["--help"], &env, repo.path());
        assert_eq!(code, 0);
        assert!(out.contains("status"), "{out}");
    }
}
