//! GitHub issue #494: a throwaway spike proving four claims about Windows job objects and
//! `jerry-pty` sessions on real hardware, ahead of stage 3 making `jerry-host` its own process.
//! Results are recorded as `docs/architecture/decisions.md` §14, not maintained here. Never
//! compiled outside test builds (`#[cfg(all(windows, test))]` in `lib.rs`) and never run in the
//! PR gate - see `docs/testing.md`'s `external` tier.
//!
//! The two `#[ignore]`d tests below are themselves the multi-process harness: each re-invokes
//! `std::env::current_exe()` (the very test binary they're running in) as a "parent" or "host"
//! role, selected by the `JERRY_SPIKE_ROLE` env var, coordinating over plain files in a shared
//! temp directory (`JERRY_SPIKE_DIR`) - simplest thing that proves the claim, no named pipe or
//! socket needed. A role subprocess is targeted by re-passing the exact, fully-qualified name of
//! the test currently running as a libtest substring filter (`JERRY_SPIKE_FILTER`), together with
//! `--ignored` (`#[ignore]`d tests are skipped by default) - guaranteed to match only that one
//! test, since two Rust items cannot share a fully-qualified name.

// This module exists entirely to call Win32 job-object/process FFI - every call site below
// carries its own SAFETY comment; see CLAUDE.md's Rust standards for the project-wide "unsafe
// only for justified FFI" rule. One module-level allow, matching `crate::job_object`'s own
// precedent for the same kind of code.
#![allow(unsafe_code)]

use std::fs;
use std::io::{self, Write as _};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::JobObjects::{
    IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

use jerry_pty::SpawnOptions;
use test_support::{wait_until, ChildGuard};

use crate::hooks::settings_file::process_is_alive;
use crate::job_object;

/// Selects which role a re-invocation of this test binary plays. Unset means "I am the
/// orchestrating test itself" - the issue's "third process".
const ROLE_VAR: &str = "JERRY_SPIKE_ROLE";
const ROLE_PARENT: &str = "parent";
const ROLE_HOST: &str = "host";
/// The shared control directory both roles read and write files in.
const DIR_VAR: &str = "JERRY_SPIKE_DIR";
/// The libtest substring filter the "parent" role reuses, unchanged, to relaunch itself as the
/// "host" role - see this module's own docs for why a fully-qualified test name is always unique.
const FILTER_VAR: &str = "JERRY_SPIKE_FILTER";
/// Selects the "parent" role's job setup. Unset (the default) mirrors the real app: a kill-on-close
/// job that permits breakaway (`crate::job_object`, decision §11). [`MODE_BREAKAWAY_FORBIDDEN`]
/// instead puts the parent in a job that does not.
const PARENT_MODE_VAR: &str = "JERRY_SPIKE_PARENT_MODE";
const MODE_BREAKAWAY_FORBIDDEN: &str = "breakaway_forbidden";

const HOST_CHILD_PID_FILE: &str = "host_child.pid";
const HOST_READY_FILE: &str = "host_ready.txt";
const HOST_OUT_FILE: &str = "host.out";
const RESIZE_REQUEST_FILE: &str = "resize_request.txt";
const RESIZE_ACK_FILE: &str = "resize_ack.txt";
const STOP_FILE: &str = "stop.flag";
const PARENT_ERROR_FILE: &str = "parent_error.txt";

/// A shell loop that prints a line once a second, indefinitely (bounded only defensively): the
/// live ConPTY output point 4 needs a third process to keep observing. `ping`, not `timeout`,
/// for the ~1s delay - `timeout` refuses to run without a real console input handle in some
/// hosts; two ICMP echoes to loopback reliably take about a second and need no such handle.
const TICKER_SCRIPT: &str =
    "for /l %i in (1,1,100000) do @(echo tick %i & ping -n 2 127.0.0.1 >nul)";

/// If this process was launched to play a role (an env var is set), plays it and returns `true` -
/// the caller must return immediately without running its own orchestrator body. Returns `false`
/// for the ordinary, top-level test invocation.
fn dispatch_role_if_requested() -> bool {
    let Ok(role) = std::env::var(ROLE_VAR) else {
        return false;
    };
    let dir = PathBuf::from(
        std::env::var(DIR_VAR).expect("JERRY_SPIKE_DIR must be set alongside JERRY_SPIKE_ROLE"),
    );
    match role.as_str() {
        ROLE_PARENT => run_parent_role(&dir),
        ROLE_HOST => run_host_role(&dir),
        other => panic!("unknown {ROLE_VAR} value: {other}"),
    }
    true
}

/// Whether the job this process currently belongs to (if any) forbids
/// `CREATE_BREAKAWAY_FROM_JOB`, read directly from the OS. `QueryInformationJobObject` with a
/// null job handle answers for *the calling process's own job*, so no handle to it is needed.
/// `Ok(false)` also covers "not in a job at all", since nothing then forbids anything.
fn breakaway_is_forbidden_for_current_process() -> io::Result<bool> {
    let mut in_any_job: BOOL = 0;
    // SAFETY: `GetCurrentProcess` returns this process's own valid pseudo-handle; a null job
    // handle asks "in any job at all"; `in_any_job` addresses a live, uniquely borrowed stack
    // `BOOL` the callee writes exactly once.
    let ok = unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &mut in_any_job) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if in_any_job == 0 {
        return Ok(false);
    }

    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: a null job handle queries the calling process's own job (confirmed above to be a
    // member of exactly one); the buffer pointer addresses a live, uniquely borrowed stack value
    // sized exactly to `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`, which the callee writes into and
    // does not retain past the call. The return-length pointer is null: the exact size asked for
    // is already known.
    let ok = unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_BREAKAWAY_OK == 0)
}

/// The specific, typed reason [`spawn_host_with_breakaway`] failed - point 2's "explicit typed
/// error, never a silent in-job spawn".
#[derive(Debug)]
enum SpikeSpawnError {
    /// The containing job does not permit `CREATE_BREAKAWAY_FROM_JOB`, confirmed by a real
    /// `QueryInformationJobObject` read of that job's own limits (not guessed from the OS error
    /// code alone). Carries the real `io::Error` the failed spawn itself returned.
    BreakawayForbidden(io::Error),
    /// Any other spawn failure - a genuinely different problem than breakaway being forbidden.
    Spawn(io::Error),
}

/// Relaunches this test binary as the "host" role with `CREATE_BREAKAWAY_FROM_JOB`, so the
/// caller's own job (if any) cannot reach it. Never silently retries without the flag on
/// failure: a forbidding job is reported back as [`SpikeSpawnError::BreakawayForbidden`] instead.
fn spawn_host_with_breakaway(filter: &str, dir: &Path) -> Result<Child, SpikeSpawnError> {
    let exe = std::env::current_exe().expect("current_exe must resolve inside a test binary");
    let mut command = jerry_pty::new_std_command(&exe);
    command
        .args([filter, "--ignored", "--test-threads=1"])
        .env(ROLE_VAR, ROLE_HOST)
        .env(DIR_VAR, dir)
        // creation_flags REPLACES what new_std_command set rather than ORing into it (see
        // `crate::updater::flow`'s identical note), so CREATE_NO_WINDOW is repeated here.
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    match command.spawn() {
        Ok(child) => Ok(child),
        Err(err) => {
            let forbidden = breakaway_is_forbidden_for_current_process()
                .expect("querying this process's own job information must succeed on real Windows");
            if forbidden {
                Err(SpikeSpawnError::BreakawayForbidden(err))
            } else {
                Err(SpikeSpawnError::Spawn(err))
            }
        }
    }
}

/// The "parent" role: joins a job (with or without breakaway permitted, per [`PARENT_MODE_VAR`]),
/// spawns the "host" role with breakaway, records the outcome to `dir`, then returns - letting
/// this process exit and its job's last handle close.
fn run_parent_role(dir: &Path) {
    let filter =
        std::env::var(FILTER_VAR).expect("JERRY_SPIKE_FILTER must be set for the parent role");
    let forbidden_mode = std::env::var(PARENT_MODE_VAR).as_deref() == Ok(MODE_BREAKAWAY_FORBIDDEN);

    if forbidden_mode {
        let job = job_object::create_job_object_with_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
            .expect("creating a breakaway-forbidding job must succeed on real Windows");
        // SAFETY: `GetCurrentProcess` takes nothing and returns this process's own pseudo-handle,
        // valid for the whole call and needing no close.
        let this_process = unsafe { GetCurrentProcess() };
        job_object::assign_process(job, this_process)
            .expect("assigning this process to its own restrictive job must succeed");
    } else {
        // Mirrors the real app's own startup (`crate::job_object::adopt_this_process`): a
        // kill-on-close job that *does* permit breakaway.
        job_object::adopt_this_process_returning_job()
            .expect("adopting this process into a breakaway-permitting job must succeed");
    }

    match spawn_host_with_breakaway(&filter, dir) {
        Ok(child) => {
            fs::write(dir.join(HOST_CHILD_PID_FILE), child.id().to_string())
                .expect("writing the spawned host's pid must succeed");
        }
        Err(err) => {
            // Matched rather than `{err:?}`'d so the real, observed `io::Error` inside each
            // variant is genuinely read - not just carried for a `Debug` impl the dead-code
            // lint doesn't count as a use.
            let message = match err {
                SpikeSpawnError::BreakawayForbidden(io_err) => {
                    format!("BreakawayForbidden: {io_err}")
                }
                SpikeSpawnError::Spawn(io_err) => format!("Spawn: {io_err}"),
            };
            fs::write(dir.join(PARENT_ERROR_FILE), message)
                .expect("recording the parent's spawn error must succeed");
        }
    }
    // No explicit job-handle cleanup: see `crate::job_object`'s own docs for why the handle this
    // function created above is deliberately never closed by hand - process exit does it.
}

/// The "host" role: gives itself its own kill-on-close job (point 3), spawns a ConPTY session
/// through `jerry_pty::spawn` (point 4), then forwards its output and services resize requests
/// via the plain-file protocol documented at this module's top, until told to stop.
fn run_host_role(dir: &Path) {
    let job = job_object::adopt_this_process_returning_job()
        .expect("the host must be able to give itself its own kill-on-close job");

    let mut session = jerry_pty::spawn(SpawnOptions::new("cmd.exe").arg("/c").arg(TICKER_SCRIPT))
        .expect("spawning the ConPTY ticker must succeed");
    let child_pid = session
        .process_id()
        .expect("a spawned Windows child must report a pid");

    let conpty_in_host_job = {
        // SAFETY: `PROCESS_QUERY_LIMITED_INFORMATION` is the access `IsProcessInJob` needs; the
        // handle is closed exactly once right after use.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child_pid) };
        if handle.is_null() {
            false
        } else {
            let mut inside: BOOL = 0;
            // SAFETY: `handle` was just returned by a successful `OpenProcess` and is still
            // live; `job` came from a successful `adopt_this_process_returning_job` above and is
            // never closed for this process's life; `inside` addresses a live, uniquely
            // borrowed stack `BOOL` written exactly once.
            let ok = unsafe { IsProcessInJob(handle, job, &mut inside) };
            // SAFETY: `handle` is closed exactly once here, after its only use above.
            unsafe { CloseHandle(handle) };
            ok != 0 && inside != 0
        }
    };

    fs::write(
        dir.join(HOST_READY_FILE),
        format!(
            "pid={}\nconpty_child_pid={child_pid}\nconpty_in_host_job={conpty_in_host_job}\n",
            std::process::id()
        ),
    )
    .expect("writing the host-ready file must succeed");

    let out_path = dir.join(HOST_OUT_FILE);
    let stop_path = dir.join(STOP_FILE);
    let resize_request_path = dir.join(RESIZE_REQUEST_FILE);
    let resize_ack_path = dir.join(RESIZE_ACK_FILE);

    // Backstop, not the intended exit path: the orchestrating test is what writes `stop.flag`,
    // but a "host" that escaped its spawner's job via breakaway is exactly the process no
    // in-process `ChildGuard`/`Drop` can reach if that orchestrator dies first (a real failure
    // mode hit once while developing this spike - see docs/architecture/decisions.md §14). A
    // bounded self-shutdown is what actually stands in for that missing teardown owner.
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut answered_cursor_query = false;
    let mut seen_tail: Vec<u8> = Vec::new();

    loop {
        if stop_path.exists() || std::time::Instant::now() >= deadline {
            break;
        }
        if let Ok(request) = fs::read_to_string(&resize_request_path) {
            let _ = fs::remove_file(&resize_request_path);
            if let Some((rows, cols)) = parse_resize_request(&request) {
                if session.resize(rows, cols).is_ok() {
                    let _ = fs::write(&resize_ack_path, format!("{rows} {cols}"));
                }
            }
        }
        // A short timeout so the loop above can keep noticing `stop`/resize requests between
        // chunks, rather than blocking indefinitely in `recv`.
        match session.output().recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => {
                if !answered_cursor_query {
                    // ConPTY's own startup Device Status Report query: it withholds further
                    // rendering output until something answers it, exactly as
                    // `jerry_pty`'s own `pty_session_tests::answer_cursor_position_query`
                    // documents - a bare `PtySession` has no terminal emulator to answer for
                    // it, which is exactly this host's situation too.
                    seen_tail.extend_from_slice(&chunk);
                    if seen_tail
                        .windows(CURSOR_POSITION_QUERY.len())
                        .any(|window| window == CURSOR_POSITION_QUERY)
                    {
                        let _ = session.write_input(CURSOR_POSITION_REPORT);
                        answered_cursor_query = true;
                    }
                }
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&out_path)
                {
                    let _ = file.write_all(&chunk);
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let _ = session.shutdown();
}

/// ConPTY's startup Device Status Report query - see [`run_host_role`]'s own docs.
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
/// The minimal valid cursor position report answering [`CURSOR_POSITION_QUERY`]: row 1, column 1.
const CURSOR_POSITION_REPORT: &[u8] = b"\x1b[1;1R";

/// Parses a `"<rows> <cols>"` resize request line.
fn parse_resize_request(text: &str) -> Option<(u16, u16)> {
    let mut parts = text.split_whitespace();
    let rows = parts.next()?.parse().ok()?;
    let cols = parts.next()?.parse().ok()?;
    Some((rows, cols))
}

/// Writes [`STOP_FILE`] into a control directory when dropped - the panic-safety half of the
/// host's teardown; see its construction site's own docs for why a plain `ChildGuard` cannot
/// stand in for it here.
struct StopHostOnDrop<'a>(&'a Path);

impl Drop for StopHostOnDrop<'_> {
    fn drop(&mut self) {
        let _ = fs::write(self.0.join(STOP_FILE), b"");
    }
}

/// Spawns this test binary as the "parent" role via `command`, already configured with
/// `ROLE_VAR`/`DIR_VAR`/`FILTER_VAR` by the caller - just the shared plumbing (stdio, guard,
/// wait) both orchestrating tests below need identically.
fn spawn_parent_role_and_wait(command: &mut std::process::Command) -> std::process::ExitStatus {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut parent = ChildGuard::spawn(command).expect("spawning the parent role must succeed");
    parent
        .wait()
        .expect("waiting for the parent role to exit must succeed")
}

/// Points 1, 3 and 4 of GitHub issue #494, in one end-to-end run: a jobbed "parent" spawns a
/// "host" with `CREATE_BREAKAWAY_FROM_JOB`, the parent exits, and this test - the "third
/// process" - keeps observing the host long after that: it is still alive, its own ConPTY
/// child is a member of a job the host itself created (not the parent's), its PTY output keeps
/// arriving, and a resize request this test issues is applied and survives.
#[test]
#[ignore = "external: windows job objects and a real ConPTY session; see docs/testing.md"]
fn host_survives_breakaway_from_a_jobbed_parent_and_keeps_streaming_its_own_pty_session() {
    if dispatch_role_if_requested() {
        return;
    }
    const SELF_FILTER: &str =
        "host_survives_breakaway_from_a_jobbed_parent_and_keeps_streaming_its_own_pty_session";

    let dir =
        tempfile::TempDir::new().expect("creating the spike's control directory must succeed");
    let exe = std::env::current_exe().expect("current_exe must resolve inside a test binary");
    let mut command = jerry_pty::new_std_command(&exe);
    command
        .args([SELF_FILTER, "--ignored", "--test-threads=1"])
        .env(ROLE_VAR, ROLE_PARENT)
        .env(DIR_VAR, dir.path())
        .env(FILTER_VAR, SELF_FILTER);
    let parent_status = spawn_parent_role_and_wait(&mut command);
    assert!(
        parent_status.success(),
        "the parent role must exit successfully after spawning the host, got {parent_status:?}"
    );

    let error_path = dir.path().join(PARENT_ERROR_FILE);
    assert!(
        !error_path.exists(),
        "the parent role must not report a spawn error in the breakaway-permitted scenario, got: {:?}",
        fs::read_to_string(&error_path)
    );

    let host_pid: u32 = fs::read_to_string(dir.path().join(HOST_CHILD_PID_FILE))
        .expect("the parent role must record the pid it spawned the host with")
        .trim()
        .parse()
        .expect("the recorded host pid must be a real number");
    // Guarantees `stop.flag` gets written even if an assertion below panics: `host` deliberately
    // escaped this test's own process tree via breakaway, so no `ChildGuard`/`Drop` in this
    // process can reach it the way `docs/testing.md`'s teardown rule otherwise expects. Declared
    // before any assertion that can panic; the loop's own 120s bound (`run_host_role`) is the
    // remaining backstop if this process itself is killed outright.
    let _stop_host_on_drop = StopHostOnDrop(dir.path());

    // POINT 1: the host survives even though its spawning "parent" process has already fully
    // exited (spawn_parent_role_and_wait already blocked until it did).
    assert!(
        process_is_alive(host_pid),
        "host pid {host_pid} must still be alive after its jobbed parent exited - this is the \
         entire claim CREATE_BREAKAWAY_FROM_JOB exists to prove"
    );

    let ready_path = dir.path().join(HOST_READY_FILE);
    assert!(
        wait_until(Duration::from_secs(10), || ready_path.exists()),
        "the host must report readiness (its own job set up, ConPTY spawned) within 10s"
    );
    let ready = fs::read_to_string(&ready_path).expect("reading the host-ready file must succeed");

    // POINT 3: the host's ConPTY child is a member of a job the host itself created after it
    // was already running as its own, breakaway-escaped process.
    assert!(
        ready.contains("conpty_in_host_job=true"),
        "the host's own ConPTY child must be a member of the host's own kill-on-close job, got: {ready:?}"
    );

    let out_path = dir.path().join(HOST_OUT_FILE);
    assert!(
        wait_until(Duration::from_secs(10), || out_path
            .metadata()
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)),
        "the host must forward some ConPTY output within 10s of spawning it"
    );
    let size_before_growth_check = out_path.metadata().expect("reading host.out's size").len();

    // POINT 4a: output keeps arriving over time - a live stream a third process keeps reading,
    // not a one-shot echo already captured above.
    assert!(
        wait_until(Duration::from_secs(10), || out_path
            .metadata()
            .map(|meta| meta.len() > size_before_growth_check)
            .unwrap_or(false)),
        "ConPTY output must keep growing over time through the surviving host"
    );

    // POINT 4b: the third process (this test) can resize the live session.
    fs::write(dir.path().join(RESIZE_REQUEST_FILE), "50 132")
        .expect("writing a resize request must succeed");
    let ack_path = dir.path().join(RESIZE_ACK_FILE);
    assert!(
        wait_until(Duration::from_secs(10), || ack_path.exists()),
        "the host must apply the resize request and acknowledge it within 10s"
    );
    let ack = fs::read_to_string(&ack_path).expect("reading the resize ack must succeed");
    assert_eq!(
        ack.trim(),
        "50 132",
        "the host must acknowledge the exact size the third process asked for"
    );

    // The session must still be alive and streaming after the resize, not broken by it.
    let size_after_resize = out_path.metadata().expect("reading host.out's size").len();
    assert!(
        wait_until(Duration::from_secs(10), || out_path
            .metadata()
            .map(|meta| meta.len() > size_after_resize)
            .unwrap_or(false)),
        "ConPTY output must keep arriving after the resize, proving it didn't break the session"
    );

    fs::write(dir.path().join(STOP_FILE), b"").expect("writing the stop file must succeed");
    assert!(
        wait_until(Duration::from_secs(10), || !process_is_alive(host_pid)),
        "the host must shut its own session down and exit once asked to stop"
    );
}

/// Point 2 of GitHub issue #494: a "parent" role that assigned *itself* into a job without
/// `JOB_OBJECT_LIMIT_BREAKAWAY_OK` - which Windows treats identically to a job an ancestor
/// process set up, since breakaway eligibility depends only on which job(s) currently contain
/// the process, not how many spawns away the job's creator was - must report the failure as
/// [`SpikeSpawnError::BreakawayForbidden`], and must never fall back to spawning the host inside
/// that job (no `host_child.pid` is ever written).
#[test]
#[ignore = "external: windows job objects; see docs/testing.md"]
fn an_outer_job_without_breakaway_ok_is_reported_as_a_typed_error_not_a_silent_spawn() {
    if dispatch_role_if_requested() {
        return;
    }
    const SELF_FILTER: &str =
        "an_outer_job_without_breakaway_ok_is_reported_as_a_typed_error_not_a_silent_spawn";

    let dir =
        tempfile::TempDir::new().expect("creating the spike's control directory must succeed");
    let exe = std::env::current_exe().expect("current_exe must resolve inside a test binary");
    let mut command = jerry_pty::new_std_command(&exe);
    command
        .args([SELF_FILTER, "--ignored", "--test-threads=1"])
        .env(ROLE_VAR, ROLE_PARENT)
        .env(DIR_VAR, dir.path())
        .env(FILTER_VAR, SELF_FILTER)
        .env(PARENT_MODE_VAR, MODE_BREAKAWAY_FORBIDDEN);
    let parent_status = spawn_parent_role_and_wait(&mut command);
    assert!(
        parent_status.success(),
        "the parent role itself must exit cleanly even though its spawn attempt fails, got {parent_status:?}"
    );

    let error = fs::read_to_string(dir.path().join(PARENT_ERROR_FILE)).expect(
        "the parent role must record a typed spawn error when its own job forbids breakaway",
    );
    assert!(
        error.contains("BreakawayForbidden"),
        "a job without JOB_OBJECT_LIMIT_BREAKAWAY_OK must be reported as the specific typed \
         error, not a generic spawn failure - got: {error}"
    );
    assert!(
        !dir.path().join(HOST_CHILD_PID_FILE).exists(),
        "a detected breakaway failure must never fall back to spawning inside the forbidding job \
         - no host pid should ever have been recorded"
    );
}
