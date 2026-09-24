//! `docs/architecture/decisions.md` §24: `jerry_core::host_spawn::spawn_or_connect` against the
//! real, compiled `jerry-host` binary rather than an in-process `Host` - this crate's own
//! `[[bin]]`, found by `jerry_core::jerry_binary::locate_named`'s one-directory-up tier (`cargo
//! test`'s integration binary lives in `target/<profile>/deps/`, the real bin one level up).
//! `CARGO_BIN_EXE_jerry-host` is only guaranteed for this crate's own integration tests
//! (`crates/jerry-host/tests/`), which is why this lives here rather than in `jerry-cli`.

// Every item in this file is test code (an integration test target) - see CLAUDE.md's Rust
// standards.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use jerry_core::client::{Client, Stream};
use jerry_core::host_spawn::{spawn_or_connect, Outcome};
use jerry_core::registry::{probe, Liveness, Registry};
use jerry_core::{
    AppCommand, AppQuery, Call, Caller, Ctx, Report, Request, SessionAttach, SessionId,
    SessionKill, SessionRecord, SessionSnapshot, SessionSpawn, SessionsQuery, Shutdown,
};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;
use test_support::{seed_empty_repo, wait_until};

/// A registry directory short enough for every platform's `sun_path`, removed on drop.
struct TestRegistry {
    registry: Registry,
    _temp: Option<tempfile::TempDir>,
}

impl Drop for TestRegistry {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.registry.dir());
    }
}

fn short_registry(tag: &str) -> TestRegistry {
    if cfg!(windows) {
        let dir = jerry_core::registry::runtime_dir()
            .expect("runtime dir")
            .join(format!("rb-{:x}-{tag}", std::process::id()));
        TestRegistry {
            registry: Registry::open(dir).expect("registry"),
            _temp: None,
        }
    } else {
        let temp = tempfile::TempDir::new().expect("tempdir");
        TestRegistry {
            registry: Registry::open(temp.path().join("r")).expect("registry"),
            _temp: Some(temp),
        }
    }
}

#[test]
fn spawn_or_connect_starts_a_real_jerry_host_binary_and_a_second_call_reuses_it() {
    let test = short_registry("real-spawn");
    let repo = seed_empty_repo();
    let common = Ctx::from_cwd(repo.path(), Caller::Human)
        .expect("a real repository")
        .repo_path;

    let first = spawn_or_connect(
        test.registry.dir().to_path_buf(),
        &common,
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
    .expect("a real jerry-host binary must spawn and publish its descriptor");
    let (first_client, descriptor) = match first {
        Outcome::Connected { client, descriptor } => (client, descriptor),
        Outcome::VersionMismatch { descriptor } => {
            panic!("unexpected version mismatch against a freshly built binary: {descriptor:?}")
        }
    };
    assert_ne!(
        descriptor.pid,
        std::process::id(),
        "the host must be a real, separate process, not something answering in-process"
    );
    drop(first_client);

    // A second spawn-or-connect for the same repository must reach the same process rather than
    // starting a duplicate (decision Q15: one host per repository).
    let second = spawn_or_connect(
        test.registry.dir().to_path_buf(),
        &common,
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
    .expect("resolving the same repository again must succeed");
    let (mut second_client, second_descriptor) = match second {
        Outcome::Connected { client, descriptor } => (client, descriptor),
        Outcome::VersionMismatch { descriptor } => {
            panic!("unexpected version mismatch on the second call: {descriptor:?}")
        }
    };
    assert_eq!(
        second_descriptor.pid, descriptor.pid,
        "the same jerry-host process must answer both spawn-or-connect calls"
    );

    // A clean `shutdown`, rather than leaving it to the default 5s linger - real subprocess
    // sprawl across a whole nextest run adds up.
    let report = second_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::Shutdown(Shutdown::default())),
        ))
        .expect("the real binary answers a real shutdown request");
    assert_eq!(
        report,
        Report::Ok {
            outcome: serde_json::Value::Null
        }
    );
    drop(second_client);

    assert!(
        wait_until(Duration::from_secs(10), || probe(&descriptor.socket)
            == Liveness::Dead),
        "the real jerry-host process must exit once told to stop"
    );
}

/// A real, interactive shell (no script, no `-c`/`/c`) - the same idiom `jerry-host`'s own
/// `session::data_plane_tests::interactive_shell_options` uses, copied rather than imported since
/// an integration test target cannot reach that crate-private module.
fn interactive_shell() -> (PathBuf, Vec<String>) {
    if cfg!(windows) {
        (PathBuf::from("cmd"), Vec::new())
    } else {
        (PathBuf::from("sh"), Vec::new())
    }
}

#[cfg(windows)]
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
#[cfg(windows)]
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// ConPTY's startup Device Status Report query - withholds all child output until *something*
/// answers it, so a bare socket client is exactly as VT-blind as this needs to be. Same shape as
/// `session::data_plane_tests::answer_cursor_position_query` and `session_manager_tests::` of the
/// same name - copied for the identical "integration test, can't reach a crate-private helper"
/// reason as [`interactive_shell`].
#[cfg(windows)]
fn answer_cursor_position_query(stream: &mut Stream, seen: &[u8], answered: &mut bool) {
    if !*answered
        && seen
            .windows(CURSOR_POSITION_QUERY.len())
            .any(|window| window == CURSOR_POSITION_QUERY)
    {
        let _ = stream.write_all(CURSOR_POSITION_REPORT);
        *answered = true;
    }
}
#[cfg(not(windows))]
fn answer_cursor_position_query(_stream: &mut Stream, _seen: &[u8], _answered: &mut bool) {}

/// Writes `line` followed by a real line ending, as if a user had typed it and pressed enter.
fn write_line(stream: &mut Stream, line: &str) {
    stream
        .write_all(format!("{line}\r\n").as_bytes())
        .expect("write a line to the data-plane socket");
}

/// Reads from `stream` until `needle` appears in the accumulated bytes, answering the Windows
/// cursor-position query along the way. Panics with what was actually seen if `overall_timeout`
/// passes first - a real failure, not a silent false negative.
fn read_until_contains(stream: &mut Stream, needle: &[u8], overall_timeout: Duration) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("set a short per-read timeout");
    let mut collected = Vec::new();
    let mut answered = false;
    let deadline = std::time::Instant::now() + overall_timeout;
    let mut buf = [0u8; 4096];
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {:?} in {:?}",
            String::from_utf8_lossy(needle),
            String::from_utf8_lossy(&collected)
        );
        match stream.read(&mut buf) {
            Ok(0) => panic!(
                "the connection closed before {:?} ever appeared in {:?}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&collected)
            ),
            Ok(n) => {
                collected.extend_from_slice(&buf[..n]);
                answer_cursor_position_query(stream, &collected, &mut answered);
                if collected
                    .windows(needle.len())
                    .any(|window| window == needle)
                {
                    return collected;
                }
            }
            Err(ref error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(error) => panic!("read error: {error}"),
        }
    }
}

/// This issue's own DoD: a session survives the process that spawned it dying, and a fresh client
/// can still reach it. Real, separate `jerry-host` binary (never an in-process stand-in), a real
/// spawned shell, real bytes over the real data-plane socket both before and after the "CLI" -
/// this test's own first `Client` and data-plane `Stream`, standing in for a real `jerry`
/// invocation or a crashed `jerry-app` - disconnects without ever sending `Shutdown` or detaching
/// cleanly. `jerry-host`'s own lifecycle already makes losing every client a *linger*, not an
/// immediate exit (decisions.md §24's `Host::run_lifecycle`) - this proves that lifecycle also
/// keeps the real session's own process, and its data plane, genuinely usable throughout, not
/// merely that the host process itself stays alive.
#[test]
fn a_session_survives_its_spawning_clients_disconnect_and_a_fresh_client_still_reaches_it() {
    let test = short_registry("survives-disconnect");
    let repo = seed_empty_repo();
    let common = Ctx::from_cwd(repo.path(), Caller::Human)
        .expect("a real repository")
        .repo_path;

    let outcome = spawn_or_connect(
        test.registry.dir().to_path_buf(),
        &common,
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
    .expect("a real jerry-host binary must spawn and publish its descriptor");
    let (mut client, descriptor) = match outcome {
        Outcome::Connected { client, descriptor } => (client, descriptor),
        Outcome::VersionMismatch { descriptor } => {
            panic!("unexpected version mismatch against a freshly built binary: {descriptor:?}")
        }
    };

    let (program, args) = interactive_shell();
    let spawn_report = client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionSpawn(SessionSpawn {
                program,
                args,
                env: Vec::new(),
                rows: 24,
                cols: 80,
                agent: None,
            })),
        ))
        .expect("the real host accepts a real spawn");
    let Report::Ok { outcome } = spawn_report else {
        panic!("expected ok, got {spawn_report:?}")
    };
    let session_id = SessionId(
        outcome["id"]
            .as_str()
            .expect("the outcome carries a real session id")
            .to_owned(),
    );

    let attach_report = client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionAttach(SessionAttach {
                id: session_id.clone(),
            })),
        ))
        .expect("attach must succeed against a session with no client yet");
    let Report::Ok { outcome } = attach_report else {
        panic!("expected ok, got {attach_report:?}")
    };
    let socket = PathBuf::from(
        outcome["socket"]
            .as_str()
            .expect("the outcome carries a real socket path"),
    );

    // Real bytes really flow, before anything "dies".
    let mut data_stream = Stream::connect(&socket).expect("connect to the real data plane");
    write_line(&mut data_stream, "echo jerry-dod-marker-before");
    read_until_contains(
        &mut data_stream,
        b"jerry-dod-marker-before",
        Duration::from_secs(15),
    );

    // The "CLI" disconnects entirely - both its data-plane attachment and its control-plane
    // client - never sending `Shutdown` and never detaching cleanly. This is exactly a crash or a
    // short-lived `jerry` invocation simply exiting, the scenario the whole cutover exists for.
    drop(data_stream);
    drop(client);

    assert_ne!(
        probe(&descriptor.socket),
        Liveness::Dead,
        "the host must survive its one client disconnecting without a real Shutdown"
    );

    // A fresh client - a new `jerry` invocation, or `jerry-app` relaunching - reconnects to the
    // same real host and finds the same session still alive.
    let mut fresh_client =
        Client::connect(&descriptor.socket, Duration::from_secs(5)).expect("reconnect");
    let sessions_report = fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Query(AppQuery::Sessions(SessionsQuery::default())),
        ))
        .expect("a fresh client must be able to query sessions");
    let Report::Ok { outcome } = sessions_report else {
        panic!("expected ok, got {sessions_report:?}")
    };
    let records: Vec<SessionRecord> =
        serde_json::from_value(outcome).expect("a real list of session records");
    assert!(
        records
            .iter()
            .any(|record| record.id == session_id && record.exit.is_none()),
        "the session must still be alive after the original client disconnected: {records:?}"
    );

    // And its real bytes still flow, through a fresh attach.
    let attach_again = fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionAttach(SessionAttach {
                id: session_id.clone(),
            })),
        ))
        .expect("re-attach must succeed now that the first client is gone");
    let Report::Ok { outcome } = attach_again else {
        panic!("expected ok, got {attach_again:?}")
    };
    let socket_again = PathBuf::from(
        outcome["socket"]
            .as_str()
            .expect("the outcome carries a real socket path"),
    );
    let mut data_stream_again =
        Stream::connect(&socket_again).expect("connect to the real data plane again");
    write_line(&mut data_stream_again, "echo jerry-dod-marker-after");
    read_until_contains(
        &mut data_stream_again,
        b"jerry-dod-marker-after",
        Duration::from_secs(15),
    );

    fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionKill(SessionKill { id: session_id })),
        ))
        .expect("kill the real session");
    fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::Shutdown(Shutdown::default())),
        ))
        .expect("shut the real host down");
    assert!(
        wait_until(Duration::from_secs(10), || probe(&descriptor.socket)
            == Liveness::Dead),
        "the real jerry-host process must exit once told to stop"
    );
}

/// Issue #507's own DoD, against the real compiled binary: output a session produces while no
/// client is attached at all - the "app" fully closed, not merely one pane detached - is still
/// visible after a fresh attach, through the real `docs/architecture/decisions.md` §25 snapshot
/// mechanism (never a raw byte replay, which never existed for a client attaching after the
/// producing bytes were already gone). `external`-tier because it spawns the real `jerry-host`
/// binary; run directly on this platform (see this crate's own builder notes) rather than only in
/// CI's dedicated nightly job.
#[test]
#[ignore = "external: jerry-host; see docs/testing.md"]
fn output_produced_while_no_client_is_attached_is_visible_in_the_snapshot_after_a_fresh_attach() {
    let test = short_registry("reattach-dod");
    let repo = seed_empty_repo();
    let common = Ctx::from_cwd(repo.path(), Caller::Human)
        .expect("a real repository")
        .repo_path;

    let outcome = spawn_or_connect(
        test.registry.dir().to_path_buf(),
        &common,
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
    .expect("a real jerry-host binary must spawn and publish its descriptor");
    let (mut client, descriptor) = match outcome {
        Outcome::Connected { client, descriptor } => (client, descriptor),
        Outcome::VersionMismatch { descriptor } => {
            panic!("unexpected version mismatch against a freshly built binary: {descriptor:?}")
        }
    };

    let (program, args) = interactive_shell();
    let spawn_report = client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionSpawn(SessionSpawn {
                program,
                args,
                env: Vec::new(),
                rows: 24,
                cols: 80,
                agent: None,
            })),
        ))
        .expect("the real host accepts a real spawn");
    let Report::Ok { outcome } = spawn_report else {
        panic!("expected ok, got {spawn_report:?}")
    };
    let session_id = SessionId(
        outcome["id"]
            .as_str()
            .expect("the outcome carries a real session id")
            .to_owned(),
    );

    // The one and only "client" (standing in for `jerry-app`) attaches once, writes the real
    // output, sees it echoed for real, then disconnects completely - control plane and data plane
    // both - and never reconnects. Everything from here on runs with genuinely no client attached
    // at all, exactly "the app closed" describes.
    let attach_report = client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionAttach(SessionAttach {
                id: session_id.clone(),
            })),
        ))
        .expect("attach must succeed against a session with no client yet");
    let Report::Ok { outcome } = attach_report else {
        panic!("expected ok, got {attach_report:?}")
    };
    let socket = PathBuf::from(
        outcome["socket"]
            .as_str()
            .expect("the outcome carries a real socket path"),
    );
    let mut data_stream = Stream::connect(&socket).expect("connect to the real data plane");
    write_line(&mut data_stream, "echo jerry-reattach-dod-marker");
    read_until_contains(
        &mut data_stream,
        b"jerry-reattach-dod-marker",
        Duration::from_secs(15),
    );
    drop(data_stream);
    drop(client);

    assert!(
        wait_until(Duration::from_secs(10), || probe(&descriptor.socket)
            != Liveness::Dead),
        "the real jerry-host process must still be alive with no client attached at all - this \
         is what makes the reattach below a real relaunch scenario, not a same-session replay"
    );

    // "Relaunch": a fresh client (`jerry-app` starting back up) reconnects and attaches again.
    // The marker must already be in the snapshot itself - painted before a single further byte
    // off this new socket is ever read - not something this test has to wait for the child to
    // print again.
    let mut fresh_client =
        Client::connect(&descriptor.socket, Duration::from_secs(5)).expect("reconnect");
    let attach_again = fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionAttach(SessionAttach {
                id: session_id.clone(),
            })),
        ))
        .expect("re-attach must succeed now that the first client is fully gone");
    let Report::Ok { outcome } = attach_again else {
        panic!("expected ok, got {attach_again:?}")
    };
    let snapshot: SessionSnapshot = serde_json::from_value(outcome["snapshot"].clone())
        .expect("the outcome carries a real snapshot");
    let painted: String = snapshot
        .cells
        .iter()
        .chain(snapshot.scrollback.iter())
        .flat_map(|row| row.iter().map(|cell| cell.c))
        .collect();
    assert!(
        painted.contains("jerry-reattach-dod-marker"),
        "output produced while no client was attached at all must be visible in the reattach \
         snapshot: {painted:?}"
    );

    fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::SessionKill(SessionKill { id: session_id })),
        ))
        .expect("kill the real session");
    fresh_client
        .request(&Call::human(
            repo.path(),
            Request::Command(AppCommand::Shutdown(Shutdown::default())),
        ))
        .expect("shut the real host down");
    assert!(
        wait_until(Duration::from_secs(10), || probe(&descriptor.socket)
            == Liveness::Dead),
        "the real jerry-host process must exit once told to stop"
    );
}
