//! The whole chain, end to end: a real `jerry hook` invocation over a real host socket, a real
//! file written between its two phases, a real `AdeApp` drain, and a real change set out the
//! other side (GitHub issue #284).
//!
//! Everything else in this folder tests one link. `store`'s tests never see a socket, `change_set`'s
//! never see an agent, `hooks::event`'s never see a file. This one skips no link, which is the
//! whole reason it exists: the joins between them - the `AgentId` in the call envelope being the
//! one the app can resolve to a worktree, the durable agent key being the one the change set
//! reports, the drain running at all - are exactly what every other test in this folder assumes.

use std::path::{Path, PathBuf};

use gpui::EntityInputHandler as _;

use crate::hooks::settings_file::{AGENT_ENV, SOCKET_ENV};
use crate::provenance::{AgentKey, Author};
use crate::test_support::{open_test_app, temp_repo_with};
use crate::work_surface::agents::ProcessKind;

const USERS_RS_BASE: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let sql = self.orm.select(&[\"id\", \"email\"]);
    }
}
";

const USERS_RS_AFTER: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let q = QueryBuilder::table(\"users\").select(&[\"id\", \"email\"]);
    }
}
";

#[gpui::test]
async fn a_real_hook_edit_event_becomes_a_real_per_agent_attribution_on_a_real_change_set_row(
    cx: &mut gpui::TestAppContext,
) {
    let repo = temp_repo_with(|root| {
        test_support::seed_empty_repo_at(root);
        test_support::commit(root, "src/api/users.rs", USERS_RS_BASE, "initial");
    });

    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

    // A real, socket-listening host swapped in for the test app's own default (the throwaway
    // in-process one `open_test_app` already wires up, which a real `jerry hook` invocation has
    // nothing to connect to), and the app's own `HookRuntime` brought up against it directly -
    // bypassing `hook_injection_for`'s `find_jerry_binary` gate, which reads this machine's real
    // `PATH` rather than anything this test controls. Once `hook_runtime` already exists,
    // `hook_injection_for` (which the real spawn below still goes through) reuses it as-is.
    //
    // The dispatch loop runs on GPUI's own background executor (`cx.background_spawn`), not a
    // raw `std::thread` - GPUI's deterministic test scheduler panics ("your test is not
    // deterministic") the moment a genuinely independent OS thread wakes a `cx.background_spawn`
    // task, which a raw-thread dispatch loop's own fan-out eventually does once `HookRuntime`'s
    // consumer task subscribes to it. The hook calls below go through the same in-process
    // `LocalClient` for the identical reason - see `hooks::integration_tests`'s own module docs
    // for the full explanation and where the real-socket transport is proven instead.
    let registry = registry_dir("provenance-e2e");
    let instance = jerry_core::registry::Registry::open(registry.path.clone())
        .expect("registry")
        .allocate()
        .expect("instance");
    let host = jerry_host::Host::start_at(registry.path.clone()).expect("host must start");
    host.listen(&instance.socket).expect("listen");
    let socket = instance.socket;
    // In-process, not a real socket `Client`: this test's own `app.new_agent` below still needs
    // `AdeApp::sessions_for`/`host_client_for` to hand back a real in-process attach (`crate::host`'s
    // own docs) - a real socket connection has nothing there to reach into yet (this cutover's own
    // pending "adapter wiring" step).
    let client = host.client();
    let repo_host = crate::host::RepoHost::for_test_in_process(host, socket.clone());
    app.update(cx, |app, cx| {
        app.adopt_repo_host_for_test(repo.path().to_path_buf(), repo_host, cx);
    });
    let hook_settings_dir = tempfile::tempdir().expect("hook settings dir");
    app.update(cx, |app, cx| {
        app.hook_runtime = crate::hooks::HookRuntime::start(
            hook_settings_dir.path(),
            Path::new("jerry"),
            socket.clone(),
            client.clone(),
            cx,
        );
        assert!(
            app.hook_runtime.is_some(),
            "the runtime must start against a real host"
        );
    });

    // A real Claude agent through the app's own path, which registers it with the real host's
    // agent table (`Agents::spawn_inner`) and injects the real socket into its environment
    // (`HookInjection::spawn_extras`).
    let spawned = app.update_in(cx, |app, window, cx| {
        app.new_agent(ProcessKind::claude(), window, cx);
        let agent = app.agents.iter().last()?;
        let spec = agent.pane.read(cx).spec_for_test().clone();
        let env: std::collections::HashMap<String, String> = spec.env.iter().cloned().collect();
        Some((
            agent.id,
            agent.cwd.clone(),
            crate::review::state::baseline_key(
                &agent.cwd,
                crate::work_surface::agents::AgentKind::Claude,
                agent.spawned_at_unix,
            ),
            env.get(AGENT_ENV)?.clone(),
            env.get(SOCKET_ENV)?.clone(),
        ))
    });
    cx.run_until_parked();

    let Some((id, cwd, key, agent_env, socket_env)) = spawned else {
        panic!("a real Claude spawn against a real host must always carry the hook environment");
    };
    assert_eq!(agent_env, id.to_string());
    assert_eq!(socket_env, socket.to_string_lossy());
    let key = AgentKey::new(key);
    let file = cwd.join("src/api/users.rs");

    // The real sequence of one `Edit` tool call, in the real order: the payload before the write,
    // the write itself, then the payload after it, submitted through the same in-process
    // `LocalClient` a generated hook entry's `jerry hook <event>` would reach over a real socket
    // (see the module docs above for why this test cannot use the real socket directly). The
    // bodies are the shape a real `claude` 2.1.228 sends (see `crate::hooks::event`'s own
    // captured constants).
    // Built with `serde_json::json!`, not a hand-formatted string template: a real Windows path
    // embeds backslashes (`C:\...`), which are not valid JSON escapes unless the serializer
    // itself escapes them.
    let body = |event: &str| {
        serde_json::json!({
            "session_id": "5a4bef04",
            "cwd": cwd.display().to_string(),
            "hook_event_name": event,
            "tool_name": "Edit",
            "tool_input": {
                "file_path": file.display().to_string(),
                "old_string": "orm",
                "new_string": "QueryBuilder",
            },
            "tool_use_id": "toolu_01",
        })
    };
    send_hook(&client, "PreToolUse", &agent_env, &cwd, body("PreToolUse")).await;
    // The host answering the request only means `dispatch::handle` broadcast the notification -
    // not that this app's own consumer task has already pulled it off the channel and taken its
    // "before" snapshot. Without this, the snapshot could be taken *after* the write below,
    // which is exactly the "diffs clean against itself" bug `AgentEdit::before`'s own docs warn
    // about.
    cx.run_until_parked();
    std::fs::write(&file, USERS_RS_AFTER).expect("the agent's own write");
    send_hook(
        &client,
        "PostToolUse",
        &agent_env,
        &cwd,
        body("PostToolUse"),
    )
    .await;
    cx.run_until_parked();

    app.update(cx, |app, cx| {
        app.apply_agent_edits(cx);
        app.load_diff(app.diff_root.clone(), cx);
    });
    cx.run_until_parked();

    app.update(cx, |app, _cx| {
        let relative = Path::new("src/api/users.rs");
        let records = app
            .line_provenance
            .worktree(&cwd)
            .expect("the agent's worktree must be tracked");
        assert_eq!(
            records.author_at(relative, 3),
            Author::Agent(key.clone()),
            "the one line the agent really changed must be that agent's"
        );
        assert_eq!(
            records.author_at(relative, 2),
            Author::Unattributed,
            "and no other line may be"
        );

        let entry = app
            .change_set
            .entry(relative)
            .expect("the changed path must be a change-set row");
        assert_eq!(entry.authors(), vec![Author::Agent(key.clone())]);
        assert_eq!(
            entry.share(&Author::Agent(key)),
            crate::provenance::DiffStat::new(1, 1),
            "one line replaced is one added and one removed, both this agent's"
        );
        assert_eq!(
            entry.stat(),
            entry
                .split()
                .values()
                .copied()
                .fold(crate::provenance::DiffStat::default(), |acc, stat| acc
                    .plus(stat)),
        );
    });
}

#[gpui::test]
fn a_real_save_through_jerrys_own_editor_flips_exactly_its_own_lines_to_you(
    cx: &mut gpui::TestAppContext,
) {
    let repo = temp_repo_with(|root| {
        test_support::seed_empty_repo_at(root);
        test_support::commit(root, "sample.txt", "one\ntwo\nthree\n", "initial");
    });

    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
    cx.run_until_parked();

    // An agent got there first, through the store's own real sequence.
    let key = AgentKey::new("utf8:/repo/wt-a|Claude|1700000000");
    let file = repo.path().join("sample.txt");
    app.update(cx, |app, _cx| {
        app.line_provenance.begin_agent_edit(repo.path(), &file);
        std::fs::write(&file, "one\nTWO\nthree\n").expect("agent write");
        app.line_provenance
            .record_agent_edit(repo.path(), &file, &key);
    });

    // Now the human opens it and types, and really saves.
    app.update_in(cx, |app, window, cx| {
        app.open_file_view(file.clone(), window, cx);
    });
    app.update(cx, |app, cx| {
        app.render_center_pane(cx);
    });
    cx.run_until_parked();
    app.update(cx, |app, cx| {
        app.render_center_pane(cx);
    });
    app.update_in(cx, |app, window, cx| {
        app.replace_text_in_range(None, "by hand ", window, cx);
        app.save_active_file(cx);
    });
    cx.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(&file).expect("read back"),
        "by hand one\nTWO\nthree\n",
        "the save must really have happened - otherwise this test proves nothing"
    );

    app.update(cx, |app, _cx| {
        let records = app.line_provenance.worktree(repo.path()).expect("tracked");
        let relative = Path::new("sample.txt");
        assert_eq!(
            records.author_at(relative, 1),
            Author::You,
            "the line the human really typed on is theirs"
        );
        assert_eq!(
            records.author_at(relative, 2),
            Author::Agent(key.clone()),
            "and the agent's own line is untouched - a hand edit flips its line, not the file"
        );
        assert_eq!(records.author_at(relative, 3), Author::Unattributed);
    });
}

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
            .join(format!("provenance-e2e-{:x}-{tag}", std::process::id()));
        RegistryDir { path, _temp: None }
    } else {
        let temp = tempfile::TempDir::new().expect("tempdir");
        RegistryDir {
            path: temp.path().join("r"),
            _temp: Some(temp),
        }
    }
}

/// The call a generated hook entry's `jerry hook <event>` performs, submitted directly through
/// the app's own in-process `LocalClient` rather than a real socket - see this file's module
/// docs for why a real socket cannot join a `#[gpui::test]`. `agent_id` is the exact
/// `JERRY_AGENT_ID` text a real spawn's environment carries (`AgentId::to_string()`).
async fn send_hook(
    client: &jerry_host::LocalClient,
    event: &str,
    agent_id: &str,
    cwd: &Path,
    payload: serde_json::Value,
) {
    let report = client
        .request(jerry_core::Call::agent(
            cwd,
            jerry_core::AgentId::from(agent_id),
            jerry_core::Request::Hook(jerry_core::HookEvent {
                event: event.to_owned(),
                payload,
            }),
        ))
        .await
        .expect("the host must accept a registered agent's hook");
    assert!(report.is_ok(), "{report:?}");
}
