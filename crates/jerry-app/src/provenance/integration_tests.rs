//! The whole chain, end to end: a real hook request on the real loopback listener, a real file
//! written between its two phases, a real `AdeApp` drain, and a real change set out the other side
//! (GitHub issue #284).
//!
//! Everything else in this folder tests one link. `store`'s tests never see a socket, `change_set`'s
//! never see an agent, `hooks::event`'s never see a file. This one skips no link, which is the
//! whole reason it exists: the joins between them - the `AgentId` in the query string being the one
//! the app can resolve to a worktree, the durable agent key being the one the change set reports,
//! the drain running at all - are exactly what every other test in this folder assumes.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;

use gpui::EntityInputHandler as _;

use crate::hooks::settings_file::{PORT_ENV, TOKEN_ENV};
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
fn a_real_hook_edit_event_becomes_a_real_per_agent_attribution_on_a_real_change_set_row(
    cx: &mut gpui::TestAppContext,
) {
    let repo = temp_repo_with(|root| {
        test_support::seed_empty_repo_at(root);
        test_support::commit(root, "src/api/users.rs", USERS_RS_BASE, "initial");
    });

    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

    // A real Claude agent through the app's own path - which is also what brings the real hook
    // listener up (`AdeApp::hook_injection_for`'s lazy, Claude-only gate).
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
            env.get(PORT_ENV)?.parse::<u16>().ok()?,
            env.get(TOKEN_ENV)?.clone(),
        ))
    });
    cx.run_until_parked();

    let Some((id, cwd, key, port, token)) = spawned else {
        // No listener means this machine could not bind loopback or write the forwarder - a real
        // state `HookRuntime::start` degrades to, and not this test's subject.
        eprintln!("skipping: no real hook runtime came up for a real Claude spawn");
        return;
    };
    let key = AgentKey::new(key);
    let file = cwd.join("src/api/users.rs");

    // The real sequence of one `Edit` tool call, in the real order: the payload before the write,
    // the write itself, then the payload after it. The bodies are the shape a real `claude`
    // 2.1.228 sends (see `crate::hooks::event`'s own captured constants).
    let body = |event: &str| {
        format!(
            r#"{{"session_id":"5a4bef04","cwd":"{cwd}","hook_event_name":"{event}","tool_name":"Edit","tool_input":{{"file_path":"{file}","old_string":"orm","new_string":"QueryBuilder"}},"tool_use_id":"toolu_01"}}"#,
            cwd = cwd.display(),
            file = file.display()
        )
    };
    post(
        port,
        &token,
        &format!("event=PreToolUse&agent={id}"),
        &body("PreToolUse"),
    );
    std::fs::write(&file, USERS_RS_AFTER).expect("the agent's own write");
    post(
        port,
        &token,
        &format!("event=PostToolUse&agent={id}"),
        &body("PostToolUse"),
    );

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

/// A real HTTP POST to the real listener, exactly as the forwarder script makes it.
fn post(port: u16, token: &str, query: &str, body: &str) {
    let request = format!(
        "POST /hook?{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to the real listener");
    stream
        .write_all(request.as_bytes())
        .expect("write the request");
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    assert!(
        response.starts_with("HTTP/1.1 204"),
        "the listener must accept a real hook request, got: {response}"
    );
}
