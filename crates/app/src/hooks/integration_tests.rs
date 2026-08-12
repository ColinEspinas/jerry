//! End-to-end tests for the agent hook side-channel (GitHub issue #239 phase 2).
//!
//! Two tiers, deliberately:
//!
//! 1. **Real transport, no `claude`.** [`the_real_forwarder_script_delivers_a_real_payload_end_to_end`]
//!    runs the *actual generated* forwarder script, as a real subprocess, with a real Claude Code
//!    payload on its stdin, against a real [`crate::hooks::server::HookListener`] on a real
//!    loopback port - and asserts the fact comes out the far end as a real
//!    [`crate::rail::status::Status`]. Everything except Claude Code itself is the production
//!    object, and it runs everywhere, always.
//!
//! 2. **Real `claude`.** The tests below it drive the genuine binary when one is installed,
//!    which is what pins Jerry's two real behavioural dependencies on it: that a `--settings`
//!    file's hooks actually fire, and that they are *merged* with the user's own rather than
//!    replacing them. Skipped (loudly) when no binary is present, and tolerant of one that is
//!    present but unusable (no auth, no network) - a sandbox without credentials must not fail
//!    the suite, but it also must not silently look like a pass, so each of those paths logs why.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::hooks::event::HookFact;
use crate::hooks::server::HookListener;
use crate::hooks::settings_file::{HookFiles, AGENT_ENV, PORT_ENV, TOKEN_ENV};
use crate::rail::status::{derive_status, HookSignal, ProcessSignal, Status, TerminalSignal};
use crate::work_surface::agents::ProcessKind;

/// A real `PreToolUse` body, captured verbatim from a real `claude` 2.1.228 run on this machine.
const REAL_PAYLOAD: &str = r#"{"session_id":"5a4bef04-9e59-4d75-874d-928b1f8c3958","cwd":"/tmp/capture","permission_mode":"default","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test --workspace","description":"Run the test suite"},"tool_use_id":"toolu_017yNzAHSe1j6rqbwMkN7gJc"}"#;

/// Blocks until `check` passes or the deadline expires - the forwarder is a real subprocess and
/// the listener a real thread, so the handoff is genuinely asynchronous.
fn wait_for(mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    check()
}

#[test]
fn the_real_forwarder_script_delivers_a_real_payload_end_to_end() {
    if !cfg!(unix) {
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let listener = HookListener::start().expect("the listener must bind a real loopback port");
    let files = HookFiles::write_in(temp.path()).expect("the generated files must be written");

    // The real generated script, named by the real generated settings file - read the command out
    // of the settings rather than reconstructing it, so this exercises the path Claude Code would
    // actually run.
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(files.settings_path()).expect("read"))
            .expect("valid JSON");
    let command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .expect("a PreToolUse command")
        .to_owned();

    let agent_id = 42;
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .env(PORT_ENV, listener.port().to_string())
        .env(TOKEN_ENV, listener.token())
        .env(AGENT_ENV, agent_id.to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("the generated hook command must be runnable");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        stdin
            .write_all(REAL_PAYLOAD.as_bytes())
            .expect("write the payload");
    }
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "the forwarder must always exit 0, got {:?}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        wait_for(|| listener.signal_for(agent_id).fact.is_some()),
        "the real payload must reach the listener through the real script"
    );

    // ...and the fact must actually change what the rail would show. This is the whole chain:
    // script -> socket -> parser -> inbox -> status derivation.
    let signal = listener.signal_for(agent_id);
    assert_eq!(signal.fact, Some(HookFact::Working));
    let (activity, question) = listener.text_for(agent_id);
    assert_eq!(activity.as_deref(), Some("Bash: cargo test --workspace"));
    assert_eq!(question, None);

    // An agent this quiet would otherwise be reported as needing input; the hook fact is what
    // makes it Run.
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
fn a_forwarder_run_outside_jerry_reaches_no_listener_at_all() {
    // The safety property that makes the generated command harmless if a user ever copies it into
    // their own settings: with no JERRY_* environment it must not post anywhere, even though a
    // real listener is running and would happily accept a correctly-tokened request.
    if !cfg!(unix) {
        return;
    }
    let temp = tempfile::tempdir().expect("temp dir");
    let listener = HookListener::start().expect("listener");
    let files = HookFiles::write_in(temp.path()).expect("files");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(files.settings_path()).expect("read"))
            .expect("valid JSON");
    let command = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .expect("a Stop command")
        .to_owned();

    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .env_remove(PORT_ENV)
        .env_remove(TOKEN_ENV)
        .env_remove(AGENT_ENV)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        let _ = stdin.write_all(REAL_PAYLOAD.as_bytes());
    }
    assert!(child.wait().expect("wait").success());

    std::thread::sleep(Duration::from_millis(300));
    for id in 0..64 {
        assert_eq!(
            listener.signal_for(id).fact,
            None,
            "an unconfigured forwarder must not report anything for any agent id"
        );
    }
}

/// Set `JERRY_REQUIRE_REAL_CLAUDE=1` to turn every "no usable `claude` here" skip below into a
/// hard failure.
///
/// Without this the real-binary tests pass vacuously wherever no binary or no credentials exist,
/// which means a green CI run says nothing at all about the two behaviours they exist to pin
/// (that `--settings` hooks fire, and that they merge with the user's own rather than replacing
/// them). Those are behaviours of a third-party binary that can change under Jerry without any
/// change to Jerry, so they need a job where the skip is not an option. This is the switch that
/// job sets; the default stays skip-friendly so a contributor without Claude Code installed can
/// still run the suite.
const REQUIRE_REAL_CLAUDE_ENV: &str = "JERRY_REQUIRE_REAL_CLAUDE";

/// Reports a skip, or panics if [`REQUIRE_REAL_CLAUDE_ENV`] demands a real run.
fn skip_or_fail(reason: &str) {
    if std::env::var(REQUIRE_REAL_CLAUDE_ENV).is_ok_and(|value| value == "1") {
        panic!("{REQUIRE_REAL_CLAUDE_ENV}=1 was set, but {reason}");
    }
    eprintln!("skipping: {reason}");
}

/// The real `claude` binary, if one is installed and looks usable.
fn real_claude() -> Option<std::path::PathBuf> {
    pty_core::resolve_on_path("claude")
}

/// Runs a real, minimal `claude` turn in `cwd` with `settings`, returning whether it succeeded.
///
/// A failure here is treated as "this environment can't run `claude`" (no credentials, no
/// network) rather than as a test failure - see the module docs.
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

#[test]
fn a_real_claude_session_reports_its_hooks_to_a_real_jerry_listener() {
    if !cfg!(unix) {
        return;
    }
    let Some(binary) = real_claude() else {
        skip_or_fail(
            "no `claude` binary on PATH - the hook transport itself is still covered by the \
             non-`claude` end-to-end test above",
        );
        return;
    };

    let temp = tempfile::tempdir().expect("temp dir");
    let listener = HookListener::start().expect("listener");
    let files = HookFiles::write_in(temp.path()).expect("files");

    // Exactly what Jerry itself would pass - built from the real production helper rather than
    // hand-assembled, so a change to either the args or the env is caught here.
    let agent_id = 7;
    let args = vec![
        "--settings".to_owned(),
        files.settings_path().to_string_lossy().into_owned(),
    ];
    let env = vec![
        (PORT_ENV.to_owned(), listener.port().to_string()),
        (TOKEN_ENV.to_owned(), listener.token().to_owned()),
        (AGENT_ENV.to_owned(), agent_id.to_string()),
    ];

    let project = temp.path().join("project");
    std::fs::create_dir_all(&project).expect("create project");

    if !run_real_claude(&binary, &project, &args, &env, None) {
        return;
    }

    assert!(
        wait_for(|| listener.signal_for(agent_id).fact.is_some()),
        "a real `claude` session run with Jerry's generated --settings must report at least one hook"
    );
    // A completed `-p` turn ends with a real `Stop`.
    assert_eq!(
        listener.signal_for(agent_id).fact,
        Some(HookFact::TurnEnded),
        "the last event of a completed turn must be the turn boundary"
    );
}

#[test]
fn jerry_s_settings_file_does_not_disable_the_user_s_own_hooks() {
    // The regression this whole feature must not cause. `--settings` merging (rather than
    // replacing) hook arrays is a real behavioural dependency on Claude Code, verified
    // empirically rather than inferred - see `crate::hooks::settings_file`'s module docs. If a
    // future Claude Code release changed this to "replace", Jerry would silently switch off
    // hooks its users configured themselves, and this test is what would catch it.
    if !cfg!(unix) {
        return;
    }
    let Some(binary) = real_claude() else {
        skip_or_fail("no `claude` binary on PATH - cannot verify --settings merge behaviour");
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
    let files = HookFiles::write_in(temp.path()).expect("files");
    let listener = HookListener::start().expect("listener");
    let args = vec![
        "--settings".to_owned(),
        files.settings_path().to_string_lossy().into_owned(),
    ];
    let env = vec![
        (PORT_ENV.to_owned(), listener.port().to_string()),
        (TOKEN_ENV.to_owned(), listener.token().to_owned()),
        (AGENT_ENV.to_owned(), "5".to_owned()),
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

    if !run_real_claude(&binary, &project, &args, &env, Some(&home)) {
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
        wait_for(|| listener.signal_for(5).fact.is_some()),
        "and Jerry's own hooks must fire too - got {fired:?}"
    );
}

/// The end-to-end test that covers what a *user* does, through the objects a user's click really
/// goes through: [`crate::root::AdeApp::new_agent`] (what the palette's "New Claude agent", the
/// Agent menu and the rail's "+" all call), a real
/// [`crate::work_surface::agents::Agents::spawn`], a real pty, a real `claude`, and a real
/// [`crate::hooks::HookRuntime`] brought up by the real lazy `hook_injection_for` gate - then
/// asserts the fact comes back out of the app's own runtime under that agent's own real id.
///
/// The tests above this one hand-assemble the `--settings` argument and the `JERRY_*` environment
/// from the production helpers and hand them to a `std::process::Command`. That pins Claude
/// Code's half of the contract and nothing at all of Jerry's: every step between a click and the
/// child process - the lazy runtime bring-up, the Claude-only gate, whether the `AgentId` in the
/// environment is the one the rail reads back, `ProcessKind::spec`, `TerminalSpec::env`,
/// `pty_core`'s `CommandBuilder` - is skipped by all of them. This one skips none of it, which is
/// the whole reason it exists: a regression anywhere along that chain would leave every other
/// test in this file green.
#[gpui::test]
fn a_claude_agent_spawned_through_the_real_app_path_really_reports_its_hooks(
    cx: &mut gpui::TestAppContext,
) {
    if !cfg!(unix) {
        return;
    }
    if real_claude().is_none() {
        skip_or_fail("no `claude` binary on PATH - the real spawn path cannot be exercised");
        return;
    }

    let repo = tempfile::tempdir().expect("tempdir");
    let (app, cx) =
        crate::root::focus::palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

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
    assert_eq!(spec.program, std::path::PathBuf::from("claude"));
    assert_eq!(
        spec.args.first().map(String::as_str),
        Some("--settings"),
        "a real Claude spawn must carry the generated settings file, got {:?}",
        spec.args
    );
    let settings_path = std::path::PathBuf::from(&spec.args[1]);
    assert!(
        settings_path.is_file(),
        "the settings path handed to `claude` must really exist on disk: {}",
        settings_path.display()
    );
    assert!(
        settings_path
            .with_file_name("jerry-hook-forwarder.sh")
            .is_file(),
        "the forwarder script the settings file names must really exist"
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
    assert!(injected.contains_key(PORT_ENV) && injected.contains_key(TOKEN_ENV));
    assert!(
        app.read_with(cx, |app, _| app.hook_runtime.is_some()),
        "the lazy runtime must have been brought up by this spawn"
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
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
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
    let started = Instant::now();
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
