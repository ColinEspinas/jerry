//! The `jerry` command. Deliberately shallow: clap builds a `Request`, the transport decides
//! whether a host serves this repository, the `Report` becomes an exit code and output.
//! Everything is a function of its arguments, the environment lookup and the cwd it is handed,
//! so the whole binary is tested without spawning it.

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cli;
pub mod exit;
pub mod transport;

use crate::cli::{Cli, Command, MergeArgs};
use crate::transport::{ChooseError, Transport};
use clap::Parser;
use jerry_core::client::{Client, ClientError};
use jerry_core::registry::{runtime_dir_for, Os, Registry};
use jerry_core::wire::rpc_code;
use jerry_core::{
    execute_locally, AgentId, AppCommand, AppQuery, Call, Caller, ConflictKind, Ctx,
    LocalDispatchError, MergeAbort, MergeAttempt, MergeAttemptOutcome, MergeComplete,
    MergeStatusOutcome, MergeStatusQuery, Report, Request, RpcError, StageResolved,
};
use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
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
    let mut session = Session {
        transport,
        ctx,
        client: None,
    };
    match &cli.command {
        Command::Status => status(&mut session, cli.json, out, err),
        Command::Merge(args) => merge(&mut session, args, cli.json, out, err),
    }
}

/// One invocation's way of reaching the executor: connected lazily on the first call, or the
/// local Git-locality path when no Jerry serves the repository.
struct Session {
    transport: Transport,
    ctx: Ctx,
    client: Option<Client>,
}

impl Session {
    /// Sends one request and returns its Report, or the exit code to stop with.
    fn call(&mut self, request: Request, err: &mut dyn Write) -> Result<Report, u8> {
        match &self.transport {
            Transport::Standalone => match execute_locally(&request, &self.ctx) {
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
            Transport::Host(_) | Transport::Socket(_) => {
                if self.client.is_none() {
                    let connected = match &self.transport {
                        Transport::Host(descriptor) => Client::connect_to(descriptor, CALL_TIMEOUT),
                        Transport::Socket(socket) => Client::connect(socket, CALL_TIMEOUT),
                        Transport::Standalone => unreachable_standalone(),
                    };
                    match connected {
                        Ok(client) => self.client = Some(client),
                        Err(error) => {
                            let _ = writeln!(
                                err,
                                "jerry: could not reach the Jerry serving this repository: {error}"
                            );
                            return Err(exit::NO_INSTANCE);
                        }
                    }
                }
                let Some(client) = self.client.as_mut() else {
                    return Err(exit::FAILED);
                };
                let call = match &self.ctx.caller {
                    Caller::Human => Call::human(&self.ctx.worktree_path, request),
                    Caller::Agent { id } => {
                        Call::agent(&self.ctx.worktree_path, id.clone(), request)
                    }
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
        }
    }

    fn describe_host(&self) -> String {
        match &self.transport {
            Transport::Host(descriptor) => format!("connected ({})", descriptor.socket.display()),
            Transport::Socket(socket) => format!("connected ({})", socket.display()),
            Transport::Standalone => "standalone (no Jerry serves this repository)".to_owned(),
        }
    }
}

/// The match above only reaches this arm for a connected transport; the type does not say so.
fn unreachable_standalone() -> Result<Client, ClientError> {
    Err(ClientError::Params(serde::de::Error::custom(
        "standalone transports never connect",
    )))
}

fn status(session: &mut Session, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let report = match session.call(Request::Query(AppQuery::Status(Default::default())), err) {
        Ok(report) => report,
        Err(code) => return code,
    };
    if json {
        return emit_json(&report, out);
    }
    match &report {
        Report::Ok { outcome } => {
            let field = |name: &str| outcome[name].as_str().unwrap_or("?").to_owned();
            let caller = match &outcome["caller"]["kind"] {
                kind if kind == "agent" => {
                    format!("agent {}", outcome["caller"]["id"].as_str().unwrap_or("?"))
                }
                _ => "human".to_owned(),
            };
            let _ = writeln!(out, "worktree    {}", field("worktree_path"));
            let _ = writeln!(out, "repository  {}", field("repo_path"));
            let _ = writeln!(out, "caller      {caller}");
            let _ = writeln!(out, "host        {}", session.describe_host());
        }
        other => explain(other, err),
    }
    exit::for_report(&report)
}

fn merge(
    session: &mut Session,
    args: &MergeArgs,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    if args.dry_run {
        return merge_dry_run(session, json, out, err);
    }
    if args.abort {
        return merge_abort(session, json, out, err);
    }
    if args.continue_ {
        return merge_continue(session, json, out, err);
    }
    merge_attempt(session, json, out, err)
}

fn merge_dry_run(
    session: &mut Session,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let report = match session.call(
        Request::Validate(AppCommand::MergeAttempt(MergeAttempt::default())),
        err,
    ) {
        Ok(report) => report,
        Err(code) => return code,
    };
    if json {
        return emit_json(&report, out);
    }
    match &report {
        Report::Ok { .. } => {
            let _ = writeln!(out, "ok: the merge can run");
        }
        other => explain(other, err),
    }
    exit::for_report(&report)
}

fn merge_attempt(
    session: &mut Session,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let report = match session.call(
        Request::Command(AppCommand::MergeAttempt(MergeAttempt::default())),
        err,
    ) {
        Ok(report) => report,
        Err(code) => return code,
    };
    let outcome: MergeAttemptOutcome = match &report {
        Report::Ok { outcome } => match serde_json::from_value(outcome.clone()) {
            Ok(outcome) => outcome,
            Err(error) => {
                let _ = writeln!(err, "jerry: unreadable merge outcome: {error}");
                return exit::FAILED;
            }
        },
        other => {
            if json {
                return emit_json(&report, out);
            }
            explain(other, err);
            return exit::for_report(&report);
        }
    };
    match outcome {
        MergeAttemptOutcome::AlreadyUpToDate { base_branch } => {
            if json {
                return emit_json(&report, out);
            }
            let _ = writeln!(
                out,
                "{base_branch} already contains this branch; nothing to merge"
            );
            exit::DONE
        }
        MergeAttemptOutcome::Clean {
            base_branch,
            base_worktree_path,
            files,
        } => {
            // A clean merge is done when it is committed; the GUI waits for a click, the
            // command line does not.
            let completed = match session.call(
                Request::Command(AppCommand::MergeComplete(MergeComplete {
                    base_worktree_path,
                })),
                err,
            ) {
                Ok(report) => report,
                Err(code) => return code,
            };
            if json {
                return emit_json(&completed, out);
            }
            match &completed {
                Report::Ok { .. } => {
                    let _ = writeln!(out, "merged into {base_branch}: {} files", files.len());
                }
                other => explain(other, err),
            }
            exit::for_report(&completed)
        }
        MergeAttemptOutcome::Conflicted {
            base_branch,
            base_worktree_path,
            conflicted,
            ..
        } => {
            if json {
                emit_json(&report, out);
                return exit::ACTION_REQUIRED;
            }
            let _ = writeln!(
                out,
                "merging into {base_branch} left {} conflicted files in {}:",
                conflicted.len(),
                base_worktree_path.display()
            );
            for entry in &conflicted {
                let how = match entry.kind {
                    ConflictKind::Text { remaining_hunks } => format!("{remaining_hunks} hunks"),
                    ConflictKind::ModifyDelete => {
                        "modified on one side, deleted on the other".into()
                    }
                    ConflictKind::Binary => "binary".into(),
                };
                let _ = writeln!(out, "  {}  ({how})", entry.path.display());
            }
            let _ = writeln!(out, "resolve them, then: jerry merge --continue");
            exit::ACTION_REQUIRED
        }
    }
}

fn merge_status(session: &mut Session, err: &mut dyn Write) -> Result<MergeStatusOutcome, u8> {
    let report = session.call(
        Request::Query(AppQuery::MergeStatus(MergeStatusQuery::default())),
        err,
    )?;
    match report {
        Report::Ok { outcome } => serde_json::from_value(outcome).map_err(|error| {
            let _ = writeln!(err, "jerry: unreadable merge status: {error}");
            exit::FAILED
        }),
        other => {
            explain(&other, err);
            Err(exit::for_report(&other))
        }
    }
}

fn merge_continue(
    session: &mut Session,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let status = match merge_status(session, err) {
        Ok(status) => status,
        Err(code) => return code,
    };
    let Some(base_worktree_path) = status.in_progress_at else {
        let _ = writeln!(err, "jerry: no merge is in progress; nothing to continue");
        return exit::DENIED;
    };
    let mut remaining: Vec<PathBuf> = Vec::new();
    for path in status.unmerged {
        let staged = match session.call(
            Request::Command(AppCommand::StageResolved(StageResolved {
                worktree_path: base_worktree_path.clone(),
                path: path.clone(),
            })),
            err,
        ) {
            Ok(report) => report,
            Err(code) => return code,
        };
        match staged {
            Report::Ok { .. } => {}
            Report::Denied { .. } => remaining.push(path),
            other => {
                if json {
                    return emit_json(&other, out);
                }
                explain(&other, err);
                return exit::for_report(&other);
            }
        }
    }
    if !remaining.is_empty() {
        if json {
            let _ = serde_json::to_writer(
                &mut *out,
                &serde_json::json!({ "status": "action-required", "unresolved": remaining }),
            );
            let _ = writeln!(out);
            return exit::ACTION_REQUIRED;
        }
        let _ = writeln!(
            out,
            "{} files still hold conflict markers:",
            remaining.len()
        );
        for path in &remaining {
            let _ = writeln!(out, "  {}", path.display());
        }
        return exit::ACTION_REQUIRED;
    }
    let completed = match session.call(
        Request::Command(AppCommand::MergeComplete(MergeComplete {
            base_worktree_path: base_worktree_path.clone(),
        })),
        err,
    ) {
        Ok(report) => report,
        Err(code) => return code,
    };
    if json {
        return emit_json(&completed, out);
    }
    match &completed {
        Report::Ok { .. } => {
            let _ = writeln!(out, "merge completed in {}", base_worktree_path.display());
        }
        other => explain(other, err),
    }
    exit::for_report(&completed)
}

fn merge_abort(session: &mut Session, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let status = match merge_status(session, err) {
        Ok(status) => status,
        Err(code) => return code,
    };
    let Some(base_worktree_path) = status.in_progress_at else {
        let _ = writeln!(err, "jerry: no merge is in progress; nothing to abort");
        return exit::DENIED;
    };
    let report = match session.call(
        Request::Command(AppCommand::MergeAbort(MergeAbort {
            base_worktree_path: base_worktree_path.clone(),
        })),
        err,
    ) {
        Ok(report) => report,
        Err(code) => return code,
    };
    if json {
        return emit_json(&report, out);
    }
    match &report {
        Report::Ok { .. } => {
            let _ = writeln!(out, "merge aborted in {}", base_worktree_path.display());
        }
        other => explain(other, err),
    }
    exit::for_report(&report)
}

fn emit_json(report: &Report, out: &mut dyn Write) -> u8 {
    if serde_json::to_writer(&mut *out, report).is_err() {
        return exit::FAILED;
    }
    let _ = writeln!(out);
    exit::for_report(report)
}

/// A refusal or failure, in words, on stderr.
fn explain(report: &Report, err: &mut dyn Write) {
    match report {
        Report::Ok { .. } => {}
        Report::Denied { code, reason } => {
            let _ = writeln!(err, "jerry: refused: {reason} ({code})");
        }
        Report::Error { error } => {
            let _ = writeln!(err, "jerry: {} ({})", error.message, error.code);
        }
    }
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
    use std::fs;
    use std::path::{Path, PathBuf};
    use test_support::{add_worktree, commit, git_output, seed_empty_repo, seed_repo, TempDir};

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

    /// A repo on `main` with `shared.txt`, and a linked worktree on `feature` kept outside
    /// the main checkout so the base stays clean.
    fn repo_with_feature() -> (TempDir, TempDir, PathBuf) {
        let repo = seed_repo();
        commit(repo.path(), "shared.txt", "base\n", "seed shared.txt");
        let outside = TempDir::new().expect("tempdir");
        let feature = outside.path().join("wt-feature");
        add_worktree(repo.path(), "feature", &feature);
        (repo, outside, feature)
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
    fn a_clean_merge_is_committed_and_exits_zero() {
        let (repo, _outside, feature) = repo_with_feature();
        commit(&feature, "new.txt", "hello\n", "add new.txt");
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);

        let (code, out, err) = invoke(&["merge", "--dry-run"], &env, &feature);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("can run"), "{out}");

        let (code, out, err) = invoke(&["merge"], &env, &feature);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("merged into main"), "{out}");
        assert!(repo.path().join("new.txt").exists());
        assert!(git_output(repo.path(), &["log", "--oneline", "-1"]).contains("Merge"));
    }

    #[test]
    fn a_conflicted_merge_exits_one_and_continue_finishes_it_once_resolved() {
        let (repo, _outside, feature) = repo_with_feature();
        commit(repo.path(), "shared.txt", "base\nmain side\n", "main edit");
        commit(
            &feature,
            "shared.txt",
            "base\nfeature side\n",
            "feature edit",
        );
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);

        let (code, out, err) = invoke(&["merge"], &env, &feature);
        assert_eq!(code, 1, "stderr: {err}\nstdout: {out}");
        assert!(out.contains("shared.txt"), "{out}");

        let (code, out, _) = invoke(&["merge", "--continue"], &env, &feature);
        assert_eq!(code, 1, "markers still there: {out}");
        assert!(out.contains("shared.txt"), "{out}");

        fs::write(repo.path().join("shared.txt"), "base\nboth\n").expect("resolve");
        let (code, out, err) = invoke(&["merge", "--continue"], &env, &feature);
        assert_eq!(code, 0, "stderr: {err}\nstdout: {out}");
        assert!(out.contains("merge completed"), "{out}");
        assert!(git_output(repo.path(), &["log", "--oneline", "-1"]).contains("Merge"));
    }

    #[test]
    fn abort_restores_the_base_and_refuses_when_nothing_is_in_progress() {
        let (repo, _outside, feature) = repo_with_feature();
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);

        let (code, _, err) = invoke(&["merge", "--abort"], &env, &feature);
        assert_eq!(code, 3, "{err}");

        commit(repo.path(), "shared.txt", "base\nmain side\n", "main edit");
        commit(
            &feature,
            "shared.txt",
            "base\nfeature side\n",
            "feature edit",
        );
        let (code, _, _) = invoke(&["merge"], &env, &feature);
        assert_eq!(code, 1);
        let (code, out, err) = invoke(&["merge", "--abort"], &env, &feature);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("merge aborted"), "{out}");
        assert!(!jerry_git::merge::merge_head_exists(repo.path()).expect("head"));
    }

    #[test]
    fn a_dirty_base_makes_the_dry_run_exit_three() {
        let (repo, _outside, feature) = repo_with_feature();
        commit(&feature, "new.txt", "hello\n", "add new.txt");
        fs::write(repo.path().join("dirty.txt"), "uncommitted\n").expect("dirty");
        let runtime = tempfile::TempDir::new().expect("runtime");
        let env = env(&[(runtime_env_key(), runtime.path())]);
        let (code, _, err) = invoke(&["merge", "--dry-run"], &env, &feature);
        assert_eq!(code, 3, "{err}");
        assert!(err.contains("merge-target-dirty"), "{err}");
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
        assert!(out.contains("merge"), "{out}");
    }
}
