//! `docs/architecture/decisions.md` §24: `jerry_core::host_spawn::spawn_or_connect` against the
//! real, compiled `jerry-host` binary rather than an in-process `Host` - this crate's own
//! `[[bin]]`, found by `jerry_core::jerry_binary::locate_named`'s one-directory-up tier (`cargo
//! test`'s integration binary lives in `target/<profile>/deps/`, the real bin one level up).
//! `CARGO_BIN_EXE_jerry-host` is only guaranteed for this crate's own integration tests
//! (`crates/jerry-host/tests/`), which is why this lives here rather than in `jerry-cli`.

// Every item in this file is test code (an integration test target) - see CLAUDE.md's Rust
// standards.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use jerry_core::host_spawn::{spawn_or_connect, Outcome};
use jerry_core::registry::{probe, Liveness, Registry};
use jerry_core::{AppCommand, Call, Caller, Ctx, Report, Request, Shutdown};
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
