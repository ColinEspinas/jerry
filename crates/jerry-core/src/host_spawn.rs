//! Spawn-or-connect to the `jerry-host` serving a repository (`docs/architecture/decisions.md`
//! §24): the one place both `jerry-app`'s `HostRuntime` and `jerry host start` resolve the
//! registry, decide whether to connect or spawn, and wait for a freshly spawned host's
//! descriptor to appear. Blocking - every real caller runs this off its own UI/foreground
//! thread. Agents never reach this: nothing here is a `Command`/`Query` an agent could invoke.

use crate::client::{Client, ClientError};
use crate::jerry_binary;
use crate::registry::{Descriptor, Registry, RegistryError, Resolution};
use crate::wire::PROTOCOL_VERSION;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum SpawnOrConnectError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// More than one live host already claims this repository - decision Q15 says this should
    /// never happen; surfaced rather than guessed at.
    #[error("{} live Jerry hosts already claim this repository", .0.len())]
    Ambiguous(Vec<Descriptor>),
    #[error("no jerry-host binary was found next to this process (run `cargo build --release -p jerry-host -p jerry-cli`)")]
    BinaryNotFound,
    /// The OS refused the detached spawn for a reason other than a forbidding job - never a
    /// silent in-process fallback (decision Q20).
    #[error("could not spawn jerry-host: {0}")]
    Spawn(#[source] io::Error),
    /// Windows only in practice: the spawning process's own job does not permit
    /// `CREATE_BREAKAWAY_FROM_JOB`, confirmed by a real OS query rather than guessed from the
    /// failed spawn's error code (`docs/architecture/decisions.md` §14). The caller's own
    /// visible "sessions cannot outlive this window" error state - never an in-job fallback.
    #[error("this window's job does not allow a session host to outlive it")]
    BreakawayForbidden,
    #[error("a freshly spawned jerry-host never published its descriptor within {0:?}")]
    NeverAppeared(Duration),
    #[error(transparent)]
    Connect(#[from] ClientError),
}

/// What [`spawn_or_connect`] found: either a real connection, or a live host whose protocol
/// version does not match this binary's own - the caller's visible error state
/// (`docs/architecture/decisions.md` §24), never connected to.
#[derive(Debug)]
pub enum Outcome {
    Connected {
        client: Client,
        descriptor: Descriptor,
    },
    VersionMismatch {
        descriptor: Descriptor,
    },
}

/// Resolves the registry for `repo`: connects to a live host already serving it (or reports a
/// version mismatch), or spawns one detached and waits up to `spawn_deadline` for its descriptor
/// to appear, polling by real connect attempts rather than a bare timer
/// (`docs/architecture/decisions.md` §24). `timeout` bounds the eventual `Client` connection
/// itself, not the wait for the descriptor.
///
/// `repo` must already be the repository's common `.git` directory (`jerry_git::
/// git_common_dir`, canonicalized), matching [`crate::registry::Registry::resolve`]'s own
/// contract - `Ctx::repo_path` already is one for every real caller (`jerry-cli`'s own `Ctx::
/// from_cwd`, `jerry-app`'s `HostRuntime::serve`). It is also the exact path the spawned
/// `jerry-host` publishes, via `--repo`.
pub fn spawn_or_connect(
    registry_dir: PathBuf,
    repo: &Path,
    timeout: Duration,
    spawn_deadline: Duration,
) -> Result<Outcome, SpawnOrConnectError> {
    // No breakaway-forbidden detection by default: that needs this workspace's sanctioned Win32
    // job-object FFI, which this crate stays free of (`docs/architecture/decisions.md` §24) - a
    // caller that can tell (`jerry-app`'s or `jerry-host`'s own `job_object::
    // breakaway_is_forbidden_for_current_process`) injects it through `spawn_or_connect_with`
    // instead. A plain `jerry host start` from a shell, with no job of its own, has nothing
    // useful to inject here anyway.
    spawn_or_connect_with(
        registry_dir,
        repo,
        timeout,
        spawn_deadline,
        jerry_binary::locate_named,
        || false,
    )
}

/// [`spawn_or_connect`], with the `jerry-host` binary lookup and the breakaway-forbidden check
/// injected. `locate_host_binary` is the seam a test drives against a fake layout rather than
/// this machine's real one, since `jerry_binary::locate_named` finds a real, workspace-built
/// `jerry-host` the moment anything else in the same `cargo nextest run --workspace` has built
/// it, which a "no binary exists" test cannot otherwise rely on staying false. `breakaway_
/// forbidden` is called only once a real spawn attempt has already failed, to classify that
/// failure - never to decide whether to attempt the spawn at all - so a caller with nothing
/// useful to say here can always pass `|| false` and just get [`SpawnOrConnectError::Spawn`].
pub fn spawn_or_connect_with(
    registry_dir: PathBuf,
    repo: &Path,
    timeout: Duration,
    spawn_deadline: Duration,
    locate_host_binary: impl Fn(&str) -> Option<PathBuf>,
    breakaway_forbidden: impl Fn() -> bool,
) -> Result<Outcome, SpawnOrConnectError> {
    let registry = Registry::open(registry_dir.clone())?;
    match registry.resolve(repo)? {
        Resolution::One(descriptor) => Ok(connect_or_flag(descriptor, timeout)?),
        Resolution::Many(descriptors) => Err(SpawnOrConnectError::Ambiguous(descriptors)),
        Resolution::None => spawn_and_wait(
            registry_dir,
            repo,
            timeout,
            spawn_deadline,
            locate_host_binary,
            breakaway_forbidden,
        ),
    }
}

fn connect_or_flag(descriptor: Descriptor, timeout: Duration) -> Result<Outcome, ClientError> {
    if descriptor.protocol_version != PROTOCOL_VERSION {
        return Ok(Outcome::VersionMismatch { descriptor });
    }
    let client = Client::connect_to(&descriptor, timeout)?;
    Ok(Outcome::Connected { client, descriptor })
}

/// Claims the right to spawn `jerry-host --repo <repo> --registry-dir <registry_dir>` detached
/// (survives this process's own exit, even from inside a kill-on-close job -
/// `jerry_pty::new_detached_command`, §14) - `Registry::claim` is what keeps two concurrent
/// callers from both spawning one (decision Q15: one host per repository) - then either spawns
/// it or, having lost the claim to a concurrent caller, waits for that caller's descriptor
/// instead. Either way, polls the registry for the descriptor by real connect attempts.
fn spawn_and_wait(
    registry_dir: PathBuf,
    repo: &Path,
    timeout: Duration,
    deadline: Duration,
    locate_host_binary: impl Fn(&str) -> Option<PathBuf>,
    breakaway_forbidden: impl Fn() -> bool,
) -> Result<Outcome, SpawnOrConnectError> {
    let registry = Registry::open(registry_dir.clone())?;
    if !registry.claim(repo, deadline)? {
        // Lost the race: someone else's claim is fresh, so their spawn is already in flight -
        // wait for their descriptor rather than spawning a second host for the same repository.
        return wait_for_descriptor(&registry, repo, timeout, deadline);
    }

    let spawned = locate_host_binary("jerry-host")
        .ok_or(SpawnOrConnectError::BinaryNotFound)
        .and_then(|binary| {
            let mut command = jerry_pty::new_detached_command(&binary);
            command
                .arg("--repo")
                .arg(repo)
                .arg("--registry-dir")
                .arg(&registry_dir);
            command.spawn().map_err(|error| {
                if breakaway_forbidden() {
                    SpawnOrConnectError::BreakawayForbidden
                } else {
                    SpawnOrConnectError::Spawn(error)
                }
            })
        });
    if let Err(error) = spawned {
        let _ = registry.release_claim(repo);
        return Err(error);
    }

    let outcome = wait_for_descriptor(&registry, repo, timeout, deadline);
    let _ = registry.release_claim(repo);
    outcome
}

/// Polls the registry for `repo`'s descriptor by real connect attempts - a real check each
/// iteration (`Registry::resolve` reads the descriptor file and probes the socket), not a bare
/// timer loop. There is no channel or event to wait on instead: a freshly spawned process
/// writing a file is not something the OS gives a cross-platform notification for.
fn wait_for_descriptor(
    registry: &Registry,
    repo: &Path,
    timeout: Duration,
    deadline: Duration,
) -> Result<Outcome, SpawnOrConnectError> {
    let started = Instant::now();
    loop {
        if let Resolution::One(descriptor) = registry.resolve(repo)? {
            return Ok(connect_or_flag(descriptor, timeout)?);
        }
        if started.elapsed() >= deadline {
            return Err(SpawnOrConnectError::NeverAppeared(deadline));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod spawn_or_connect_tests {
    use super::{spawn_or_connect, spawn_or_connect_with, Outcome, SpawnOrConnectError};
    use crate::client::Listener;
    use crate::registry::{Descriptor, Registry};
    use std::path::PathBuf;
    use std::time::Duration;
    use test_support::seed_empty_repo;

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
            let dir = crate::registry::runtime_dir()
                .expect("runtime dir")
                .join(format!("sc-{:x}-{tag}", std::process::id()));
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
    fn a_live_matching_host_is_connected_to_without_spawning_anything() {
        let test = short_registry("connect");
        let repo = seed_empty_repo();
        let instance = test.registry.allocate().expect("allocate");
        let _listener = Listener::bind(&instance.socket).expect("bind");
        test.registry
            .publish(&instance, &[repo.path().to_path_buf()])
            .expect("publish");

        let outcome = spawn_or_connect(
            test.registry.dir().to_path_buf(),
            repo.path(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect("connects to the already-live host");
        assert!(matches!(outcome, Outcome::Connected { .. }));
    }

    #[test]
    fn a_version_mismatched_host_is_flagged_rather_than_connected_to() {
        let test = short_registry("mismatch");
        let repo = seed_empty_repo();
        let instance = test.registry.allocate().expect("allocate");
        let _listener = Listener::bind(&instance.socket).expect("bind");
        let descriptor = Descriptor {
            protocol_version: crate::wire::PROTOCOL_VERSION + 1,
            pid: std::process::id(),
            started_at: 0,
            repos: vec![std::fs::canonicalize(repo.path()).expect("canonical")],
            socket: instance.socket.clone(),
        };
        std::fs::write(
            &instance.descriptor,
            serde_json::to_vec_pretty(&descriptor).expect("json"),
        )
        .expect("write descriptor directly, bypassing Registry::publish's own version stamp");

        let outcome = spawn_or_connect(
            test.registry.dir().to_path_buf(),
            repo.path(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect("resolves, does not error");
        match outcome {
            Outcome::VersionMismatch { descriptor: got } => {
                assert_eq!(got.protocol_version, crate::wire::PROTOCOL_VERSION + 1);
            }
            other => panic!("expected a version mismatch, got {other:?}"),
        }
    }

    /// A fake binary locator that never finds anything, so the "spawn" half is exercised without
    /// a real process - the typed `BinaryNotFound` error, not a hang or a panic. The real
    /// `jerry_binary::locate_named` cannot be relied on to fail here: a `cargo nextest run
    /// --workspace` run builds `jerry-host`'s own `[[bin]]` into the same shared `target/`
    /// directory this test binary lives under, which its "one directory up" tier then finds -
    /// this test hit exactly that once, spawning and leaking a real `jerry-host` process instead
    /// of exercising the error path it claims to.
    #[test]
    fn no_live_host_and_no_jerry_host_binary_is_a_typed_error_not_a_hang() {
        let test = short_registry("nobinary");
        let repo = seed_empty_repo();
        let err = spawn_or_connect_with(
            test.registry.dir().to_path_buf(),
            repo.path(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            |_name| None,
            || false,
        )
        .expect_err("no host and no binary to spawn one with");
        assert!(
            matches!(err, SpawnOrConnectError::BinaryNotFound),
            "{err:?}"
        );
    }

    /// The `breakaway_forbidden` injection point: it is consulted only once a real spawn
    /// attempt has already failed (a nonexistent binary path is a real, guaranteed failure), and
    /// its answer alone decides `BreakawayForbidden` vs. the generic `Spawn` error - this crate
    /// itself has no opinion, and never touches Win32 FFI to form one
    /// (`docs/architecture/decisions.md` §24).
    #[test]
    fn a_forbidding_job_is_reported_as_breakaway_forbidden_when_the_injected_check_says_so() {
        let test = short_registry("breakaway");
        let repo = seed_empty_repo();
        let fake_binary = PathBuf::from("/definitely/does/not/exist/jerry-host");
        let err = spawn_or_connect_with(
            test.registry.dir().to_path_buf(),
            repo.path(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            move |_name| Some(fake_binary.clone()),
            || true,
        )
        .expect_err("a nonexistent binary path always fails to spawn");
        assert!(
            matches!(err, SpawnOrConnectError::BreakawayForbidden),
            "{err:?}"
        );
    }

    /// The same failed spawn, with the injected check saying breakaway is not the reason - the
    /// generic error, not a false `BreakawayForbidden`.
    #[test]
    fn a_spawn_failure_is_generic_when_the_injected_check_says_breakaway_is_not_the_reason() {
        let test = short_registry("generic-spawn-failure");
        let repo = seed_empty_repo();
        let fake_binary = PathBuf::from("/definitely/does/not/exist/jerry-host");
        let err = spawn_or_connect_with(
            test.registry.dir().to_path_buf(),
            repo.path(),
            Duration::from_secs(5),
            Duration::from_millis(200),
            move |_name| Some(fake_binary.clone()),
            || false,
        )
        .expect_err("a nonexistent binary path always fails to spawn");
        assert!(matches!(err, SpawnOrConnectError::Spawn(_)), "{err:?}");
    }

    #[test]
    fn more_than_one_live_host_is_reported_as_ambiguous() {
        let test = short_registry("ambiguous");
        let repo = seed_empty_repo();
        let repos = vec![repo.path().to_path_buf()];
        // Both listeners must stay alive for the whole test - one dropped per loop iteration
        // (rather than collected here) would leave only one live host, not two.
        let mut listeners = Vec::new();
        for _ in 0..2 {
            let instance = test.registry.allocate().expect("allocate");
            listeners.push(Listener::bind(&instance.socket).expect("bind"));
            test.registry.publish(&instance, &repos).expect("publish");
        }

        let err = spawn_or_connect(
            test.registry.dir().to_path_buf(),
            repo.path(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .expect_err("two live hosts must not be picked between silently");
        assert!(
            matches!(err, SpawnOrConnectError::Ambiguous(ref many) if many.len() == 2),
            "{err:?}"
        );
    }
}
