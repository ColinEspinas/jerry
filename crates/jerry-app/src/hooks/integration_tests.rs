//! End-to-end tests for the agent hook side-channel (GitHub issue #239 phase 2; decision Q10,
//! `docs/architecture/decisions.md` §19: `jerry hook` replaces the curl forwarder).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AppContext as _, TestAppContext};

use crate::hooks::event::HookFact;
use crate::hooks::settings_file::{AGENT_ENV, SOCKET_ENV};
use crate::rail::status::{derive_status, HookSignal, ProcessSignal, Status, TerminalSignal};
use crate::test_support::open_test_app;
use crate::work_surface::agents::{AgentKind, ProcessKind};

/// A registry directory this test's own host publishes into - short enough for every platform's
/// socket path limit, removed on drop even when the test fails. Mirrors
/// `crate::host::app_dispatch_tests`'s own identically-shaped helper.
struct RegistryDir {
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

impl Drop for RegistryDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn registry_dir(tag: &str) -> RegistryDir {
    if cfg!(windows) {
        let path = jerry_core::registry::runtime_dir()
            .expect("runtime dir")
            .join(format!("hook-e2e-{:x}-{tag}", std::process::id()));
        RegistryDir { path, _temp: None }
    } else {
        let temp = tempfile::TempDir::new().expect("tempdir");
        RegistryDir {
            path: temp.path().join("r"),
            _temp: Some(temp),
        }
    }
}

/// A bare socket path (not a `Registry` directory) short enough for every platform's `sun_path`,
/// removed on drop even when the test fails - for the two `external` tests below, which bind a
/// raw `jerry_host::Host::listen` rather than going through registry allocation.
struct SocketPath {
    path: PathBuf,
    _temp: Option<tempfile::TempDir>,
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn socket_path(tag: &str) -> SocketPath {
    if cfg!(windows) {
        let dir = jerry_core::registry::runtime_dir().expect("runtime dir");
        std::fs::create_dir_all(&dir).expect("runtime dir exists");
        SocketPath {
            path: dir.join(format!("hook-e2e-{}-{tag}.sock", std::process::id())),
            _temp: None,
        }
    } else {
        let temp = tempfile::TempDir::new().expect("tempdir");
        SocketPath {
            path: temp.path().join(format!("{tag}.sock")),
            _temp: Some(temp),
        }
    }
}

/// A fake environment lookup closed over `pairs` - `jerry_cli::run`'s own shape.
fn fake_env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), OsString::from(*value)))
        .collect();
    move |key: &str| map.get(key).cloned()
}

/// The real path a generated hook entry's `jerry hook <event>` invocation takes, driven
/// in-process rather than as a real subprocess: no real `jerry` binary is available to a unit
/// test, but `jerry_cli::run` *is* the real transport all the way down to a real socket. Returns
/// `(exit code, stdout, stderr)`.
fn run_jerry_hook(
    event: &str,
    socket: &Path,
    agent_id: crate::work_surface::agents::AgentId,
    cwd: &Path,
    payload: &[u8],
) -> (u8, Vec<u8>, Vec<u8>) {
    let socket_str = socket.to_str().expect("utf8 socket path");
    let agent_str = agent_id.to_string();
    let env = fake_env(&[(SOCKET_ENV, socket_str), (AGENT_ENV, &agent_str)]);
    let mut out = Vec::new();
    let mut err = Vec::new();
    // Owned: `jerry_cli::run` takes stdin by value now, since `hook`'s bounded read moves it
    // onto its own thread rather than borrowing it.
    let stdin: Box<dyn std::io::Read + Send> = Box::new(std::io::Cursor::new(payload.to_vec()));
    let code = jerry_cli::run(
        ["jerry", "hook", event].map(OsString::from),
        &env,
        cwd,
        stdin,
        &mut out,
        &mut err,
    );
    (code, out, err)
}

/// Real socket, real `jerry_host::Host` dispatch thread, real `jerry_cli::run` - deliberately a
/// plain `#[test]`, never a `#[gpui::test]`. GPUI's deterministic test scheduler panics
/// ("Detected activity on thread ..., but test scheduler is running on ...: your test is not
/// deterministic") the moment a genuinely independent OS thread wakes a `cx.background_spawn`
/// task - which a real socket's listener/dispatch threads always eventually do once a request
/// actually completes. `crates/jerry-host`'s own accept loop is real OS threads by design
/// (decisions.md §15: "`jerry-core` owns no threads. The listener, dispatch task and session
/// table are `jerry-host`'s"), so this half of the contract - the wire transport - is proven
/// here, against the host's real, thread-driven dispatch, exactly like `jerry-host`'s and
/// `jerry-cli`'s own test suites already do. The other half - the app's own `HookRuntime`
/// consumer task correctly recording what a notification carries - is proven separately below,
/// through the app's in-process `LocalClient`, which never crosses a real thread boundary and so
/// stays inside a `#[gpui::test]` safely.
#[test]
fn jerry_hook_reaches_a_real_host_over_a_real_socket_and_the_notification_can_be_recorded() {
    let repo = test_support::seed_empty_repo();
    let socket = socket_path("real-transport");
    let host = jerry_host::Host::start().expect("host");
    host.listen(&socket.path).expect("listen");
    let agent_id: crate::work_surface::agents::AgentId = 42;
    host.agents().register(
        jerry_core::AgentId::from(agent_id.to_string()),
        repo.path().to_path_buf(),
        "Claude".into(),
    );
    let mut events = host.client().subscribe();

    // The real invocation a generated hook entry performs, with exactly the environment
    // `HookInjection::spawn_extras`/`env_only` would have injected into the agent's own process.
    let (code, out, err) = run_jerry_hook(
        "PreToolUse",
        &socket.path,
        agent_id,
        repo.path(),
        br#"{"tool_name":"Bash","tool_input":{"command":"cargo test --workspace"}}"#,
    );
    assert_eq!(code, 0, "stderr: {}", String::from_utf8_lossy(&err));
    assert!(out.is_empty());

    let mut received = None;
    assert!(
        test_support::wait_until(Duration::from_secs(5), || {
            received = events.try_recv().ok();
            received.is_some()
        }),
        "the host must have fanned out an event/hook notification"
    );
    let jerry_core::Message::Notification { method, params } = received.expect("received") else {
        panic!("expected a notification");
    };
    assert_eq!(method, "event/hook");
    assert_eq!(params["agent"], serde_json::json!(agent_id.to_string()));
    assert_eq!(params["event"], serde_json::json!("PreToolUse"));

    // Exactly what `HookRuntime`'s own consumer task does with each notification - the free
    // function both it and this test call, so a change to the parsing/recording logic is caught
    // here even though the consumer's own async plumbing cannot safely be driven by a real socket
    // inside a deterministic test.
    let inbox = std::sync::Mutex::new(crate::hooks::inbox::HookInbox::default());
    let edits = std::sync::Mutex::new(crate::hooks::inbox::EditLog::default());
    super::record_hook_notification(&inbox, &edits, params);
    let inbox = inbox.into_inner().expect("lock");
    let record = inbox
        .get(agent_id)
        .expect("the real round-tripped fact must have been recorded");
    assert_eq!(record.report.fact, HookFact::Working);
    assert_eq!(
        record.report.activity.as_deref(),
        Some("Bash: cargo test --workspace")
    );

    host.shutdown_and_join();
}

/// The app's own `HookRuntime` consumer task, driven through the same in-process `LocalClient`
/// `AdeApp::dispatch` itself uses - so this stays fully inside GPUI's deterministic executor
/// (see the plain test above for why a real socket cannot join this one). A real, socket-backed
/// `HostRuntime` is still swapped in for `open_test_app`'s own unpublished default so
/// `HookInjection::env_only`'s `JERRY_HOST_SOCKET` string names a real path, matching production;
/// nothing here ever connects to that socket.
#[gpui::test]
async fn a_hook_dispatched_through_the_apps_own_host_reaches_its_hook_runtimes_consumer_task(
    cx: &mut TestAppContext,
) {
    let repo = test_support::seed_empty_repo();
    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

    let registry = registry_dir("host");
    let (runtime, dispatch) =
        crate::host::HostRuntime::start(registry.path.clone()).expect("host must start");
    let socket = runtime.socket().to_path_buf();
    let agent_id: crate::work_surface::agents::AgentId = 42;
    let client = app.update(cx, |app, cx| {
        cx.background_spawn(dispatch).detach();
        if let Some(agents) = runtime.agents() {
            agents.register(
                jerry_core::AgentId::from(agent_id.to_string()),
                repo.path().to_path_buf(),
                "Claude".into(),
            );
        }
        app.adopt_host(runtime, cx);
        app.host_runtime
            .as_ref()
            .and_then(crate::host::HostRuntime::client)
            .expect("the just-adopted host has a client")
    });

    // The app's own `HookRuntime`, brought up exactly as `hook_injection_for` would - but
    // without going through its `find_jerry_binary` gate, which reads this machine's real `PATH`
    // and this test binary's real location rather than anything this test controls. The path
    // only ever ends up embedded in generated text; it is never actually executed here.
    let hook_settings_dir = tempfile::tempdir().expect("hook settings dir");
    app.update(cx, |app, cx| {
        app.hook_runtime = crate::hooks::HookRuntime::start(
            hook_settings_dir.path(),
            Path::new("jerry"),
            socket,
            client.clone(),
            cx,
        );
        assert!(
            app.hook_runtime.is_some(),
            "the runtime must start against a real, reachable host"
        );
    });
    // The consumer task's first poll is what actually calls `LocalClient::subscribe` and
    // registers it with the host's fanout; without this, the request below could broadcast its
    // notification before anything is listening for it, and the message is gone rather than
    // queued for a subscriber that arrives later.
    cx.run_until_parked();

    // The same call a generated hook entry's `jerry hook <event>` performs over a real socket,
    // submitted here directly through the in-process `LocalClient` instead - see the module docs
    // for why a real socket cannot join a `#[gpui::test]`.
    let report = client
        .request(jerry_core::Call::agent(
            repo.path(),
            jerry_core::AgentId::from(agent_id.to_string()),
            jerry_core::Request::Hook(jerry_core::HookEvent {
                event: "PreToolUse".to_owned(),
                payload: serde_json::json!({
                    "tool_name": "Bash",
                    "tool_input": { "command": "cargo test --workspace" },
                }),
            }),
        ))
        .await
        .expect("the host must accept a registered agent's hook");
    assert!(report.is_ok(), "{report:?}");

    cx.run_until_parked();

    let signal = app.read_with(cx, |app, _| {
        app.hook_runtime
            .as_ref()
            .expect("runtime")
            .signal_for(agent_id)
    });
    assert_eq!(signal.fact, Some(HookFact::Working));
    let (activity, question) = app.read_with(cx, |app, _| {
        app.hook_runtime
            .as_ref()
            .expect("runtime")
            .text_for(agent_id)
    });
    assert_eq!(activity.as_deref(), Some("Bash: cargo test --workspace"));
    assert_eq!(question, None);

    // ...and the fact must actually change what the rail would show. This is the whole new
    // chain: a hook request -> the real host's fanout -> this app's own notification-consuming
    // task -> the inbox -> status derivation.
    let long_quiet = ProcessSignal::Running {
        idle: Duration::from_secs(600),
    };
    assert_eq!(
        derive_status(
            ProcessKind::claude(),
            long_quiet,
            TerminalSignal::default(),
            HookSignal::default(),
            false
        ),
        Status::Ask,
        "baseline: without the hook this agent's silence reads as needing input"
    );
    assert_eq!(
        derive_status(
            ProcessKind::claude(),
            long_quiet,
            TerminalSignal::default(),
            signal,
            false
        ),
        Status::Run,
        "the real round-tripped hook fact must be what decides the status"
    );
}

#[test]
fn a_hook_from_an_unregistered_agent_is_refused_but_never_touches_stdout() {
    // The safety property that makes the generated command harmless if it somehow ran with a
    // stale or forged `JERRY_AGENT_ID`: the host refuses it (`FORBIDDEN`), and `jerry hook`
    // still exits 0 having printed nothing, exactly as it must for a dead listener under the old
    // transport. A plain test (real socket, real thread) for the same reason the transport test
    // above is one - see its own docs.
    let repo = test_support::seed_empty_repo();
    let socket = socket_path("unregistered");
    let host = jerry_host::Host::start().expect("host");
    host.listen(&socket.path).expect("listen");

    let (code, out, _err) = run_jerry_hook(
        "Stop",
        &socket.path,
        99,
        repo.path(),
        br#"{"hook_event_name":"Stop"}"#,
    );
    assert_eq!(
        code, 0,
        "a hook must never fail even when the host refuses it"
    );
    assert!(out.is_empty());
    host.shutdown_and_join();
}

/// Set `JERRY_REQUIRE_REAL_CLAUDE=1` to turn every "no usable `claude`/`jerry` here" skip below
/// into a hard failure.
const REQUIRE_REAL_CLAUDE_ENV: &str = "JERRY_REQUIRE_REAL_CLAUDE";

/// Reports a skip, or panics if [`REQUIRE_REAL_CLAUDE_ENV`] demands a real run.
fn skip_or_fail(reason: &str) {
    if std::env::var(REQUIRE_REAL_CLAUDE_ENV).is_ok_and(|value| value == "1") {
        panic!("{REQUIRE_REAL_CLAUDE_ENV}=1 was set, but {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// The real `claude` binary, if one is installed and looks usable.
fn real_claude() -> Option<PathBuf> {
    jerry_pty::resolve_on_path("claude")
}

/// The real `jerry` binary, if one is reachable - unlike the in-process tests above, the two
/// tests below spawn a real `claude`, which then spawns a real *subprocess* for its hook, so
/// only a real file on disk will do. Cargo sets `CARGO_BIN_EXE_jerry` for this crate's own test
/// binary because `jerry-cli` is now a dev-dependency of `crate::hooks::integration_tests`'s own
/// crate (`jerry-app`'s `Cargo.toml`); `PATH` is the fallback for a manual run.
fn real_jerry_binary() -> Option<PathBuf> {
    option_env!("CARGO_BIN_EXE_jerry")
        .map(PathBuf::from)
        .or_else(|| jerry_pty::resolve_on_path("jerry"))
}

/// Runs a real, minimal `claude` turn in `cwd` with `settings`, returning whether it succeeded.
fn run_real_claude(
    binary: &Path,
    cwd: &Path,
    args: &[String],
    env: &[(String, String)],
    home: Option<&Path>,
) -> bool {
    let mut command = std::process::Command::new(binary);
    command
        .args(args)
        .arg("-p")
        .arg("reply with the single word ok")
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(home) = home {
        command.env("HOME", home);
    }
    match command.output() {
        Ok(output) => {
            if !output.status.success() {
                skip_or_fail(&format!(
                    "the installed `claude` could not complete a turn here ({:?}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            output.status.success()
        }
        Err(err) => {
            skip_or_fail(&format!("could not run `claude` ({err})"));
            false
        }
    }
}

/// Drains every notification already buffered on `events`, returning the last one - a completed
/// `claude` turn has already finished posting every hook it will ever post by the time
/// `run_real_claude` returns, so there is nothing left to race.
fn last_buffered(
    events: &mut futures::channel::mpsc::UnboundedReceiver<jerry_core::Message>,
) -> Option<jerry_core::Message> {
    let mut last = None;
    while let Ok(message) = events.try_recv() {
        last = Some(message);
    }
    last
}

#[ignore = "external: claude, jerry; see docs/testing.md"]
#[test]
fn a_real_claude_session_reports_its_hooks_to_a_real_jerry_host() {
    let Some(claude) = real_claude() else {
        skip_or_fail(
            "no `claude` binary on PATH - the hook transport itself is still covered by the \
             non-`claude` end-to-end tests above",
        );
        return;
    };
    let Some(jerry) = real_jerry_binary() else {
        skip_or_fail(
            "no `jerry` binary reachable - cannot generate a real, runnable settings file",
        );
        return;
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let socket = socket_path("real-claude");
    let host = jerry_host::Host::start().expect("host");
    host.listen(&socket.path).expect("listen");
    let agent_id = 7u64;
    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");
    host.agents().register(
        jerry_core::AgentId::from(agent_id.to_string()),
        project.clone(),
        "Claude".into(),
    );
    let mut events = host.client().subscribe();

    // Exactly what Jerry itself would pass - built from the real production helper rather than
    // hand-assembled, so a change to either the args or the env is caught here.
    let files = crate::hooks::settings_file::HookFiles::write_in(temp.path(), &jerry)
        .expect("files must write");
    let args = vec![
        "--settings".to_owned(),
        files.settings_path().to_string_lossy().into_owned(),
    ];
    let env = vec![
        (AGENT_ENV.to_owned(), agent_id.to_string()),
        (
            SOCKET_ENV.to_owned(),
            socket.path.to_string_lossy().into_owned(),
        ),
    ];

    if !run_real_claude(&claude, &project, &args, &env, None) {
        return;
    }

    let mut last = None;
    assert!(
        test_support::wait_until(Duration::from_secs(10), || {
            last = last_buffered(&mut events).or(last.take());
            last.is_some()
        }),
        "a real `claude` session run with Jerry's generated --settings must report at least one hook"
    );
    // A completed `-p` turn ends with a real `Stop`.
    match last.expect("received") {
        jerry_core::Message::Notification { method, params } => {
            assert_eq!(method, "event/hook");
            assert_eq!(params["agent"], serde_json::json!(agent_id.to_string()));
            assert_eq!(
                params["event"], "Stop",
                "the last event of a completed turn must be the turn boundary"
            );
        }
        other => panic!("expected a notification, got {other:?}"),
    }
    host.shutdown_and_join();
}

#[ignore = "external: claude, jerry; see docs/testing.md"]
#[test]
fn jerry_s_settings_file_does_not_disable_the_user_s_own_hooks() {
    // The regression this whole feature must not cause. `--settings` merging (rather than
    // replacing) hook arrays is a real behavioural dependency on Claude Code, verified
    // empirically rather than inferred. If a future Claude Code release changed this to
    // "replace", Jerry would silently switch off hooks its users configured themselves, and
    // this test is what would catch it.
    //
    // Still Unix-only, and for a reason that is about the *test*, not about Jerry: the two
    // stand-in "user" hooks it plants are `echo ... >> <file>` shell commands, and it redirects
    // `HOME` to keep the real `~/.claude` untouched. Neither has a one-line Windows equivalent,
    // and what is being pinned here is Claude Code's *merge* behaviour, which is a property of
    // Claude Code rather than of the platform.
    if !cfg!(unix) {
        return;
    }
    let Some(claude) = real_claude() else {
        skip_or_fail("no `claude` binary on PATH - cannot verify --settings merge behaviour");
        return;
    };
    let Some(jerry) = real_jerry_binary() else {
        skip_or_fail(
            "no `jerry` binary reachable - cannot generate a real, runnable settings file",
        );
        return;
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(home.join(".claude")).expect("create home");
    std::fs::create_dir_all(project.join(".claude")).expect("create project");

    let marker = temp.path().join("fired.txt");
    let hook_settings = |label: &str| {
        format!(
            r#"{{"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"echo {label} >> {}"}}]}}]}}}}"#,
            marker.display()
        )
    };
    std::fs::write(home.join(".claude/settings.json"), hook_settings("USER")).expect("write user");
    std::fs::write(
        project.join(".claude/settings.json"),
        hook_settings("PROJECT"),
    )
    .expect("write project");

    // Jerry's real generated file, written outside the project exactly as in production.
    let files = crate::hooks::settings_file::HookFiles::write_in(temp.path(), &jerry)
        .expect("files must write");
    let socket = socket_path("merge-check");
    let host = jerry_host::Host::start().expect("host");
    host.listen(&socket.path).expect("listen");
    let agent_id = 5u64;
    host.agents().register(
        jerry_core::AgentId::from(agent_id.to_string()),
        project.clone(),
        "Claude".into(),
    );
    let mut events = host.client().subscribe();
    let args = vec![
        "--settings".to_owned(),
        files.settings_path().to_string_lossy().into_owned(),
    ];
    let env = vec![
        (AGENT_ENV.to_owned(), agent_id.to_string()),
        (
            SOCKET_ENV.to_owned(),
            socket.path.to_string_lossy().into_owned(),
        ),
    ];

    // A temp HOME so the real `~/.claude/settings.json` is never touched. Credentials are copied
    // across so the session can still authenticate; without them this simply skips.
    if let Some(real_home) = std::env::var_os("HOME") {
        let real_home = Path::new(&real_home);
        let _ = std::fs::copy(
            real_home.join(".claude/.credentials.json"),
            home.join(".claude/.credentials.json"),
        );
        let _ = std::fs::copy(real_home.join(".claude.json"), home.join(".claude.json"));
    }

    if !run_real_claude(&claude, &project, &args, &env, Some(&home)) {
        return;
    }

    let fired = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        fired.contains("USER"),
        "the user's own ~/.claude hook must still fire alongside Jerry's - got {fired:?}"
    );
    assert!(
        fired.contains("PROJECT"),
        "the project's own .claude hook must still fire alongside Jerry's - got {fired:?}"
    );
    assert!(
        test_support::wait_until(Duration::from_secs(10), || last_buffered(&mut events)
            .is_some()),
        "and Jerry's own hooks must fire too - got {fired:?}"
    );
    host.shutdown_and_join();
}

/// The end-to-end test that covers what a *user* does, through the objects a user's click really
/// goes through: [`crate::root::AdeApp::new_agent`] (what the palette's "New Claude agent", the
/// Agent menu and the rail's "+" all call), a real
/// [`crate::work_surface::agents::Agents::spawn`], a real pty, a real `claude`, and a real
/// [`crate::hooks::HookRuntime`] brought up by the real lazy `hook_injection_for` gate - then
/// asserts the fact comes back out of the app's own runtime under that agent's own real id.
///
/// Needs a real, socket-listening host swapped in for the test app's own default
/// (`HostRuntime::in_process`, which a spawned `jerry hook` subprocess has nothing to connect
/// to) and a real, locatable `jerry` binary - both graceful skips, matching `real_claude()`'s own
/// shape, rather than a hard requirement of this test file.
#[ignore = "external: claude, jerry; see docs/testing.md"]
#[gpui::test]
async fn a_claude_agent_spawned_through_the_real_app_path_really_reports_its_hooks(
    cx: &mut gpui::TestAppContext,
) {
    let Some(_claude) = real_claude() else {
        skip_or_fail("no `claude` binary on PATH - the real spawn path cannot be exercised");
        return;
    };
    let Some(jerry) = real_jerry_binary() else {
        skip_or_fail("no `jerry` binary reachable - hook injection would be silently disabled");
        return;
    };

    let repo = tempfile::tempdir().expect("tempdir");
    let (app, cx) =
        crate::root::focus::palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

    let registry = registry_dir("real-app-path");
    let (runtime, dispatch) =
        crate::host::HostRuntime::start(registry.path.clone()).expect("host must start");
    std::thread::spawn(move || futures::executor::block_on(dispatch));
    app.update(cx, |app, cx| app.adopt_host(runtime, cx));
    // Every `ui`-tier fixture stubs every agent kind's binary (GitHub issue #530) - undo it here,
    // since this is the one test that must really exec `claude` for its own real path to mean
    // anything.
    app.update(cx, |app, _cx| {
        app.agents.clear_binary_override(AgentKind::Claude)
    });

    let (id, pane) = app.update_in(cx, |app, window, cx| {
        app.new_agent(ProcessKind::claude(), window, cx);
        let agent = app
            .agents
            .iter()
            .last()
            .expect("the agent `new_agent` just spawned");
        (agent.id, agent.pane.clone())
    });
    cx.run_until_parked();

    // The real command line and environment this spawn produced - asserted from the pane the app
    // itself built, not from a reconstruction of what it ought to have built.
    let spec = pane.read_with(cx, |pane, _| pane.spec_for_test().clone());
    assert_eq!(spec.program, PathBuf::from("claude"));
    assert_eq!(
        spec.args.first().map(String::as_str),
        Some("--settings"),
        "a real Claude spawn must carry the generated settings file, got {:?}",
        spec.args
    );
    let settings_path = PathBuf::from(&spec.args[1]);
    assert!(
        settings_path.is_file(),
        "the settings path handed to `claude` must really exist on disk: {}",
        settings_path.display()
    );
    let injected: std::collections::HashMap<&str, &str> = spec
        .env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    assert_eq!(
        injected.get(AGENT_ENV).copied(),
        Some(id.to_string().as_str()),
        "the environment must name the same agent id the rail reads this agent's status back under"
    );
    assert!(
        injected.contains_key(SOCKET_ENV),
        "the environment must name the real host socket, or the spawned `jerry hook` has \
         nothing to connect to"
    );
    assert!(
        app.read_with(cx, |app, _| app.hook_runtime.is_some()),
        "the lazy runtime must have been brought up by this spawn, which needs a locatable \
         `jerry` binary: {}",
        jerry.display()
    );

    // Claude Code will not start a session in a directory it has never seen until a human answers
    // "do you trust this folder?", and until it does, *no hook fires at all*. Jerry's whole
    // product is spawning agents into freshly created worktrees, so that screen is the normal
    // first thing a real agent shows, not an edge case. Answer it with a real keystroke on the
    // real pane, exactly as a user does.
    fn pump(cx: &mut gpui::VisualTestContext, rounds: usize) {
        for _ in 0..rounds {
            cx.background_executor
                .advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        pump(cx, 10);
        let asking = pane.read_with(cx, |pane, _| {
            pane.visible_text_lines()
                .iter()
                .any(|line| line.contains("trust this folder"))
        });
        if asking {
            cx.simulate_keystrokes("enter");
            pump(cx, 10);
            break;
        }
    }

    // `SessionStart` fires as soon as the session really starts, so nothing has to be typed.
    let started = std::time::Instant::now();
    let mut fact = None;
    while started.elapsed() < Duration::from_secs(90) && fact.is_none() {
        pump(cx, 4);
        fact = app.read_with(cx, |app, _| {
            app.hook_runtime
                .as_ref()
                .and_then(|runtime| runtime.signal_for(id).fact)
        });
    }

    if fact.is_none() {
        // A sandbox with no credentials can't start a session at all - the same "`claude` is
        // installed but unusable here" case the tests above tolerate. Reported, never silently
        // passed off as a green run.
        let screen = pane.read_with(cx, |pane, _| pane.visible_text_lines().join("\n"));
        app.update_in(cx, |app, window, cx| app.close_agent(id, window, cx));
        skip_or_fail(&format!(
            "the installed `claude` never started a session through the real spawn path; the \
             pane showed:\n{}",
            screen.trim()
        ));
        return;
    }

    assert_eq!(
        fact,
        Some(HookFact::Working),
        "a session that has really started reports itself as working"
    );
    // And the real session id GitHub issue #227's resume path needs comes back the same way.
    assert!(
        app.read_with(cx, |app, _| app
            .hook_runtime
            .as_ref()
            .and_then(|runtime| runtime.session_id_for(id))
            .is_some()),
        "the real Claude Code session id must reach the app through the real path too"
    );

    app.update_in(cx, |app, window, cx| app.close_agent(id, window, cx));
    cx.run_until_parked();
}

/// Issue #502's definition of done: a real, autonomous `claude` session, told nothing but a
/// prompt, uses `jerry wt new --agent claude` on its own and a real `event/worktree-created`
/// notification (`agent: "claude"`) reaches the host over a real socket - the same real transport
/// [`a_real_claude_session_reports_its_hooks_to_a_real_jerry_host`] proves for hooks. That a real
/// `AdeApp` really spawns a second agent in reaction to this exact notification is proven
/// separately, in `crate::work_surface::worktree_created::tests` (the in-process host, matching
/// this file's own documented "real socket, or the app's consumer - never both in the same test"
/// split, decisions.md §19); this test's own job is only the first half - that a real agent
/// really reaches for `jerry wt new` and the real host really hears about it.
#[ignore = "external: claude, jerry; see docs/testing.md"]
#[test]
fn a_real_claude_session_uses_jerry_wt_new_and_the_real_host_hears_about_it() {
    let Some(claude) = real_claude() else {
        skip_or_fail(
            "no `claude` binary on PATH - the worktree-create transport itself is still covered \
             by jerry-core's and jerry-host's own test suites",
        );
        return;
    };
    let Some(jerry) = real_jerry_binary() else {
        skip_or_fail(
            "no `jerry` binary reachable - cannot generate a real, runnable settings file",
        );
        return;
    };

    let repo = test_support::seed_repo();
    let socket = socket_path("wt-new-agent");
    let host = jerry_host::Host::start().expect("host");
    host.listen(&socket.path).expect("listen");
    let agent_id = 13u64;
    host.agents().register(
        jerry_core::AgentId::from(agent_id.to_string()),
        repo.path().to_path_buf(),
        "Claude".into(),
    );
    let mut events = host.client().subscribe();

    let settings_temp = tempfile::tempdir().expect("temp dir");
    let files = crate::hooks::settings_file::HookFiles::write_in(settings_temp.path(), &jerry)
        .expect("files must write");
    let jerry_dir = jerry.parent().expect("jerry has a parent directory");
    let path_with_jerry = std::env::join_paths(
        std::iter::once(jerry_dir.to_path_buf()).chain(
            std::env::var_os("PATH")
                .as_deref()
                .map(std::env::split_paths)
                .into_iter()
                .flatten(),
        ),
    )
    .expect("PATH joins");
    // No `--dangerously-skip-permissions`, unlike a full permission-bypassed agent: `--allowedTools`
    // alone is what every other real-`claude` test in this file relies on. Not verified here that
    // headless `-p` mode honors it without also skipping permissions outright - if this test is
    // ever actually run and stalls on a permission prompt instead of completing, that is the first
    // thing to check.
    let args = vec![
        "--settings".to_owned(),
        files.settings_path().to_string_lossy().into_owned(),
        "--plugin-dir".to_owned(),
        files.plugin_dir().to_string_lossy().into_owned(),
        "--allowedTools".to_owned(),
        "Bash(jerry:*)".to_owned(),
    ];
    let env = vec![
        (AGENT_ENV.to_owned(), agent_id.to_string()),
        (
            SOCKET_ENV.to_owned(),
            socket.path.to_string_lossy().into_owned(),
        ),
        (
            "PATH".to_owned(),
            path_with_jerry.to_string_lossy().into_owned(),
        ),
    ];

    if !run_real_claude_with_prompt(
        &claude,
        repo.path(),
        &args,
        &env,
        "Run this exact shell command and nothing else: jerry wt new wt-new-agent-e2e --agent claude",
    ) {
        return;
    }

    let mut last = None;
    assert!(
        test_support::wait_until(Duration::from_secs(15), || {
            while let Ok(message) = events.try_recv() {
                if let jerry_core::Message::Notification { method, .. } = &message {
                    if method == "event/worktree-created" {
                        last = Some(message);
                    }
                }
            }
            last.is_some()
        }),
        "a real claude session told to run `jerry wt new --agent claude` must produce a real \
         event/worktree-created notification"
    );
    match last.expect("received") {
        jerry_core::Message::Notification { params, .. } => {
            assert_eq!(params["agent"], serde_json::json!("claude"));
            let path = params["path"].as_str().expect("path");
            assert!(Path::new(path).is_dir(), "{path}");
            let _ = std::fs::remove_dir_all(Path::new(path).parent().expect("parent"));
        }
        other => panic!("expected a notification, got {other:?}"),
    }
    host.shutdown_and_join();
}

/// [`run_real_claude`], with an explicit `prompt` rather than the fixed "reply with the single
/// word ok" - for a test that needs `claude` to actually take an action, not just answer.
fn run_real_claude_with_prompt(
    binary: &Path,
    cwd: &Path,
    args: &[String],
    env: &[(String, String)],
    prompt: &str,
) -> bool {
    let mut command = std::process::Command::new(binary);
    command
        .args(args)
        .arg("-p")
        .arg(prompt)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    match command.output() {
        Ok(output) => {
            if !output.status.success() {
                skip_or_fail(&format!(
                    "the installed `claude` could not complete a turn here ({:?}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            output.status.success()
        }
        Err(err) => {
            skip_or_fail(&format!("could not run `claude` ({err})"));
            false
        }
    }
}

/// [`run_real_claude_with_prompt`], returning the captured stdout instead of a bool - this test
/// needs to see what a real tool call actually printed, not just whether the process exited 0.
fn run_real_claude_capturing_stdout(
    binary: &Path,
    cwd: &Path,
    args: &[String],
    prompt: &str,
) -> Option<String> {
    let mut command = std::process::Command::new(binary);
    command
        .args(args)
        .arg("-p")
        .arg(prompt)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match command.output() {
        Ok(output) => {
            if !output.status.success() {
                skip_or_fail(&format!(
                    "the installed `claude` could not complete a turn here ({:?}): {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr)
                ));
                return None;
            }
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        Err(err) => {
            skip_or_fail(&format!("could not run `claude` ({err})"));
            None
        }
    }
}

/// Issue #509's definition of done: a real, autonomous `claude` session launched with nothing but
/// the per-launch plugin directory (`--plugin-dir`, the same directory `.claude-plugin/plugin.json`
/// and `SKILL.md` already live in) can see and call a real `jerry` MCP tool over stdio - proving
/// `.mcp.json`'s auto-load (`docs/architecture/decisions.md` §22) really works end to end, not
/// just that the generated file has the right shape (`hooks::settings_file::tests`' own job,
/// unit-tier). No `JERRY_HOST_SOCKET`/host needed: `query_status` is Git-locality and answers
/// standalone.
#[ignore = "external: claude, jerry; see docs/testing.md"]
#[test]
fn a_real_claude_session_lists_and_calls_a_real_jerry_mcp_tool() {
    let Some(claude) = real_claude() else {
        skip_or_fail(
            "no `claude` binary on PATH - the MCP server itself is still covered by jerry-cli's \
             own in-process test suite",
        );
        return;
    };
    let Some(jerry) = real_jerry_binary() else {
        skip_or_fail("no `jerry` binary reachable - cannot generate a real, runnable .mcp.json");
        return;
    };

    let repo = test_support::seed_repo();
    let settings_temp = tempfile::tempdir().expect("temp dir");
    let files = crate::hooks::settings_file::HookFiles::write_in(settings_temp.path(), &jerry)
        .expect("files must write");
    let args = vec![
        "--settings".to_owned(),
        files.settings_path().to_string_lossy().into_owned(),
        "--plugin-dir".to_owned(),
        files.plugin_dir().to_string_lossy().into_owned(),
        "--allowedTools".to_owned(),
        // The MCP-tool allow-rule syntax (verified at
        // <https://code.claude.com/docs/en/permissions>): `mcp__<server>__<tool>` for one tool. A
        // server declared by a *plugin's* `.mcp.json` (rather than a project one) is additionally
        // namespaced `plugin_<plugin name>_<server name>` - confirmed against a real denial
        // message naming the tool exactly this way (both our plugin and our server are named
        // "jerry", hence the doubled segment); `query_status` is `query/status`'s own tool name
        // (`jerry_core::mcp::tool_name`).
        "mcp__plugin_jerry_jerry__query_status".to_owned(),
    ];

    let Some(stdout) = run_real_claude_capturing_stdout(
        &claude,
        repo.path(),
        &args,
        "Call the jerry MCP tool that reports status - your worktree, repository and caller - \
         and print its raw JSON result, and nothing else.",
    ) else {
        return;
    };
    assert!(
        stdout.contains("worktree_path"),
        "expected the real jerry `query_status` tool's own outcome field in the transcript: \
         {stdout}"
    );
}
