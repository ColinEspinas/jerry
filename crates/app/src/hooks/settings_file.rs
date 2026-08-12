//! Generating the real `--settings` file and forwarder script Jerry hands a spawned `claude`
//! (GitHub issue #239, phase 2).
//!
//! ## `--settings` merges hook arrays, it does not replace them - verified, not assumed
//!
//! The dangerous version of this feature is the one where Jerry's generated settings file
//! silently disables hooks the *user* configured. That would be a real regression: someone's
//! formatter-on-write or commit-guard hook quietly stopping the moment they open the agent in
//! Jerry, with no error and no clue why.
//!
//! So it was tested against a real `claude` binary (2.1.228) on a real scratch project rather
//! than inferred from the docs, whose wording on the point is indirect. Three hooks were declared
//! on the same `SessionStart` event at three different layers - user (`~/.claude/settings.json`,
//! via a temp `HOME`), project (`.claude/settings.json`), and a Jerry-style `--settings` file
//! outside the project - each appending a distinct marker to a file. A real session was run.
//! **All three markers were written**: hook arrays are merged across every settings layer, and a
//! `--settings` file adds to them rather than overriding them.
//!
//! That is why nothing here reads or splices the user's existing settings: doing so would be
//! *worse* than useless. Claude Code already merges, so a Jerry file that also copied the user's
//! hooks in would run every one of them twice.
//!
//! This is a real behavioural dependency on Claude Code, so it is pinned by a real test
//! (`crate::hooks::integration_tests`) that runs the actual binary when one is installed.
//!
//! ## Why a script rather than an inline one-liner
//!
//! The settings file names one small script, written once per launch and shared by every hook
//! entry and every agent, instead of embedding a shell pipeline in each of nine `command`
//! strings. The nine entries then differ only by the event name they pass as `$1`, which keeps
//! the quoting problem to exactly one place.
//!
//! ## Why the forwarder is dumb, and why that is the safety property
//!
//! It reads stdin, POSTs it verbatim, and exits 0. It parses no JSON - all extraction happens in
//! Jerry's own Rust ([`crate::hooks::event`]) with a real parser, because a shell script picking
//! fields out of untrusted JSON with `sed` is exactly how a payload becomes a command.
//!
//! Two properties are load-bearing:
//!
//! - **It no-ops outside Jerry.** If `JERRY_HOOK_PORT`/`JERRY_HOOK_TOKEN`/`JERRY_AGENT_ID` are
//!   not all set, it exits 0 immediately, having done nothing. Those variables are injected on
//!   the spawned process (see `crate::terminal::pane::TerminalSpec::env`), so they exist only
//!   inside a pane Jerry spawned. If a user ever copies the generated command into their own
//!   settings, or runs the script by hand, it does nothing at all rather than posting their
//!   session's payloads at whatever now answers on that port.
//! - **It always exits 0.** A hook's exit code is not advisory - exit code 2 *blocks the tool
//!   call*, and any other non-zero code surfaces a "hook error" to the user. If Jerry has since
//!   quit, or the port is dead, or `curl` isn't installed, the agent must carry on completely
//!   unaffected. `curl`'s status is therefore discarded rather than propagated: the worst
//!   outcome of a broken listener is that Jerry falls back to the Phase 1 heuristics, never that
//!   the user's agent stops working.

use std::io;
use std::path::{Path, PathBuf};

/// The hook events Jerry declares. Every one is a real, current Claude Code event
/// (<https://code.claude.com/docs/en/hooks>), and every one maps to a real
/// [`crate::hooks::event::HookFact`] - Jerry declares nothing it does not act on, so a user
/// inspecting the generated file sees exactly the surface Jerry actually uses.
pub const FORWARDED_EVENTS: [&str; 9] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Notification",
    "Stop",
    "StopFailure",
];

/// Env var carrying the loopback listener's real port.
pub const PORT_ENV: &str = "JERRY_HOOK_PORT";
/// Env var carrying this launch's auth token.
pub const TOKEN_ENV: &str = "JERRY_HOOK_TOKEN";
/// Env var carrying the pane's `crate::work_surface::agents::AgentId`.
pub const AGENT_ENV: &str = "JERRY_AGENT_ID";

/// The forwarder script's file name inside the launch directory.
const FORWARDER_NAME: &str = "jerry-hook-forwarder.sh";
/// The generated settings file's name inside the launch directory.
const SETTINGS_NAME: &str = "jerry-hook-settings.json";

/// The POSIX `sh` forwarder - see the module docs for why it is shaped exactly like this.
///
/// `$1` is the event name, supplied per hook entry by the generated settings file.
const FORWARDER_SCRIPT: &str = r#"#!/bin/sh
# Written by Jerry (github.com/ColinEspinas/jerry) to forward Claude Code hook payloads to the
# Jerry instance that spawned this agent. Safe to run anywhere: without the JERRY_* environment
# variables Jerry injects on the panes it spawns, this exits immediately having done nothing.
[ -n "$JERRY_HOOK_PORT" ] || exit 0
[ -n "$JERRY_HOOK_TOKEN" ] || exit 0
[ -n "$JERRY_AGENT_ID" ] || exit 0
[ -n "$1" ] || exit 0
command -v curl >/dev/null 2>&1 || exit 0

# Never propagate curl's exit status: a non-zero hook exit blocks the agent's tool call (exit 2)
# or shows the user a hook error. A dead listener must cost nothing.
curl --silent --show-error --output /dev/null --max-time 5 \
  --request POST \
  --header "Authorization: Bearer $JERRY_HOOK_TOKEN" \
  --header "Content-Type: application/json" \
  --data-binary @- \
  "http://127.0.0.1:$JERRY_HOOK_PORT/hook?event=$1&agent=$JERRY_AGENT_ID" >/dev/null 2>&1

exit 0
"#;

/// The real on-disk files backing one Jerry launch's hook injection. Removed on drop.
#[derive(Debug)]
pub struct HookFiles {
    directory: PathBuf,
    settings: PathBuf,
}

impl HookFiles {
    /// The path to pass as `claude --settings <path>`.
    pub fn settings_path(&self) -> &Path {
        &self.settings
    }

    /// Writes this launch's forwarder script and settings file into a fresh private directory.
    ///
    /// `parent` is the directory to create the launch directory inside - the OS temp directory in
    /// production. Deliberately *not* `~/.claude/settings.json` or any path Claude Code reads on
    /// its own: this file must only ever affect sessions Jerry itself spawned with an explicit
    /// `--settings`, so a `claude` the user starts from their own terminal is completely
    /// untouched by Jerry having been installed.
    pub fn write_in(parent: &Path) -> io::Result<HookFiles> {
        // Process id *and* a process-global counter, exactly like
        // `crate::review::baseline_state::ReviewBaselineState::save_at`'s temp-file naming and for
        // the identical reason: several `AdeApp` instances genuinely share one process (GitHub
        // issue #90's "New Window", and every test that builds more than one app). A pid-only name
        // would make all of them share one directory - so the second instance's `write_in` would
        // delete the first's files out from under its running agents, and whichever instance
        // closed first would remove the directory the other was still using, silently killing its
        // hooks for the rest of the session.
        static INSTANCE_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let instance = INSTANCE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = parent.join(format!("jerry-hooks-{}-{instance}", std::process::id()));
        // A stale directory left by a dead process that happened to reuse this pid is removed
        // rather than merged into, so a leftover file can never be reused with a dead launch's
        // token.
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory)?;
        restrict_to_owner(&directory, 0o700)?;

        let forwarder = directory.join(FORWARDER_NAME);
        std::fs::write(&forwarder, FORWARDER_SCRIPT)?;
        // Executable, but only by this user - the settings file next to it holds the token.
        restrict_to_owner(&forwarder, 0o700)?;

        let settings = directory.join(SETTINGS_NAME);
        std::fs::write(&settings, settings_json(&forwarder)?)?;
        // Not executable, and readable only by this user: this file names the forwarder, and the
        // directory it lives in is the thing an attacker would want to tamper with.
        restrict_to_owner(&settings, 0o600)?;

        Ok(HookFiles {
            directory,
            settings,
        })
    }
}

impl Drop for HookFiles {
    /// Removes the whole launch directory. Best-effort: a failure here is not worth surfacing (a
    /// leftover directory in the OS temp directory is harmless - it holds no secret, since the
    /// token lives only in this process's memory and its children's environments, never in these
    /// files), and `Drop` cannot report one anyway.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Sets owner-only permissions on Unix. A no-op elsewhere - see [`is_supported`] for why this
/// path isn't reached on Windows at all.
#[cfg(unix)]
fn restrict_to_owner(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Whether Jerry can install hooks on this platform at all.
///
/// Unix only, and honestly so: the forwarder is a POSIX `sh` script, and Claude Code would need a
/// different (`cmd`/PowerShell) forwarder plus different quoting on Windows. Rather than write a
/// Windows path that has never been run against a real `claude`, hook injection is skipped there
/// entirely and every agent falls back to the Phase 1 title/OSC and quiescence signals - which
/// work identically on all platforms. This is a real, documented gap, not a silent failure.
pub const fn is_supported() -> bool {
    cfg!(unix)
}

/// Builds the real `--settings` JSON declaring every [`FORWARDED_EVENTS`] entry against
/// `forwarder`.
///
/// Built with `serde_json` rather than string formatting so the script's path is escaped
/// correctly no matter what it contains. The path is additionally single-quoted *within* the
/// shell command string, because Claude Code runs a `command` hook through a shell - an unquoted
/// path containing a space would otherwise be split into a wrong program plus stray arguments.
fn settings_json(forwarder: &Path) -> io::Result<String> {
    let quoted = shell_quote(&forwarder.to_string_lossy());
    let mut hooks = serde_json::Map::new();
    for event in FORWARDED_EVENTS {
        hooks.insert(
            event.to_string(),
            serde_json::json!([{
                "hooks": [{
                    "type": "command",
                    "command": format!("{quoted} {event}"),
                }]
            }]),
        );
    }
    let document = serde_json::json!({ "hooks": serde_json::Value::Object(hooks) });
    serde_json::to_string_pretty(&document)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Wraps `value` in POSIX single quotes, escaping any single quote inside it via the standard
/// `'\''` idiom. Single quotes are used rather than double because inside them the shell expands
/// nothing at all - so a path containing `$`, backticks or `\` is passed through literally.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generated_settings_declare_every_forwarded_event_against_the_real_script() {
        let temp = tempfile::tempdir().expect("temp dir");
        let files = HookFiles::write_in(temp.path()).expect("must write");

        let raw = std::fs::read_to_string(files.settings_path()).expect("settings must exist");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("must be valid JSON");
        let hooks = parsed
            .get("hooks")
            .and_then(serde_json::Value::as_object)
            .expect("a hooks object");

        assert_eq!(
            hooks.len(),
            FORWARDED_EVENTS.len(),
            "Jerry must declare exactly the events it acts on, no more"
        );
        for event in FORWARDED_EVENTS {
            let command = hooks
                .get(event)
                .and_then(|entries| entries.get(0))
                .and_then(|entry| entry.get("hooks"))
                .and_then(|entries| entries.get(0))
                .and_then(|hook| hook.get("command"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_else(|| panic!("{event} must declare a command hook"));
            assert!(
                command.ends_with(&format!(" {event}")),
                "{event}: the event name must be passed to the forwarder, got {command:?}"
            );
            assert!(
                command.contains(FORWARDER_NAME),
                "{event}: must point at the real generated script, got {command:?}"
            );
        }
    }

    #[test]
    fn the_generated_files_really_exist_and_the_script_is_executable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let files = HookFiles::write_in(temp.path()).expect("must write");
        assert!(files.settings_path().is_file());

        let forwarder = files.settings_path().with_file_name(FORWARDER_NAME);
        assert!(forwarder.is_file(), "the forwarder script must be written");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script_mode = forwarder.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(
                script_mode, 0o700,
                "the script must be owner-only executable"
            );
            let settings_mode = files
                .settings_path()
                .metadata()
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                settings_mode, 0o600,
                "the settings file must be owner-read/write only"
            );
        }
    }

    #[test]
    fn the_auth_token_is_never_written_into_any_generated_file() {
        // The token lives only in this process's memory and in the environment of the children
        // Jerry spawns - never on disk. That is what keeps a leaked or undeleted temp directory
        // (a `SIGKILL`ed Jerry leaves one behind) from being a credential leak, and it is what
        // `HookFiles::drop`'s "these files hold no secret" reasoning depends on. A refactor that
        // moved the token into the settings file would silently invalidate both.
        let temp = tempfile::tempdir().expect("temp dir");
        let listener = crate::hooks::server::HookListener::start().expect("listener");
        let token = listener.token().to_owned();
        let files = HookFiles::write_in(temp.path()).expect("files");

        let settings = std::fs::read_to_string(files.settings_path()).expect("read settings");
        let forwarder =
            std::fs::read_to_string(files.settings_path().with_file_name(FORWARDER_NAME))
                .expect("read forwarder");

        assert!(
            !settings.contains(&token),
            "the settings file must never contain the auth token"
        );
        assert!(
            !forwarder.contains(&token),
            "the forwarder script must never contain the auth token - it reads it from the environment"
        );
        // It must genuinely read the token from the environment instead.
        assert!(
            forwarder.contains(TOKEN_ENV),
            "the forwarder must take the token from ${TOKEN_ENV}"
        );
    }

    #[test]
    fn two_instances_in_one_process_never_share_or_delete_each_other_s_files() {
        // GitHub issue #90's "New Window" puts two real `AdeApp`s in one process. With a
        // pid-only directory name they collided: the second's `write_in` deleted the first's
        // files, and the first's `Drop` then removed the directory the second was still using -
        // silently killing hook delivery for every agent in that window.
        let temp = tempfile::tempdir().expect("temp dir");
        let first = HookFiles::write_in(temp.path()).expect("first");
        let second = HookFiles::write_in(temp.path()).expect("second");

        assert_ne!(
            first.directory, second.directory,
            "two instances in one process must not share a launch directory"
        );
        assert!(first.settings_path().is_file());
        assert!(second.settings_path().is_file());

        let second_settings = second.settings_path().to_path_buf();
        let second_forwarder = second_settings.with_file_name(FORWARDER_NAME);
        drop(first);
        assert!(
            second_settings.is_file(),
            "closing one window must not delete the other's settings file"
        );
        assert!(
            second_forwarder.is_file(),
            "closing one window must not delete the other's forwarder script"
        );
    }

    #[test]
    fn dropping_the_files_removes_the_whole_launch_directory() {
        let temp = tempfile::tempdir().expect("temp dir");
        let settings_path;
        let directory;
        {
            let files = HookFiles::write_in(temp.path()).expect("must write");
            settings_path = files.settings_path().to_path_buf();
            directory = files.directory.clone();
            assert!(settings_path.exists());
        }
        assert!(
            !settings_path.exists(),
            "the generated settings file must not outlive the launch"
        );
        assert!(!directory.exists(), "the launch directory must be removed");
    }

    #[test]
    fn the_forwarder_no_ops_without_jerry_env_vars_and_never_reports_failure() {
        // The property that makes the generated command safe to run outside Jerry (see the module
        // docs). Run the real script, with a real JSON payload on stdin, with no JERRY_* set.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let files = HookFiles::write_in(temp.path()).expect("must write");
        let forwarder = files.settings_path().with_file_name(FORWARDER_NAME);

        let output = std::process::Command::new("/bin/sh")
            .arg(&forwarder)
            .arg("Stop")
            .env_remove(PORT_ENV)
            .env_remove(TOKEN_ENV)
            .env_remove(AGENT_ENV)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("the forwarder must be runnable");
        assert!(
            output.status.success(),
            "the forwarder must exit 0 outside Jerry, got {:?}",
            output.status
        );
        assert!(
            output.stdout.is_empty(),
            "it must produce no output that Claude Code would feed back as context"
        );
    }

    #[test]
    fn the_forwarder_exits_zero_even_when_the_listener_is_dead() {
        // A hook that exits non-zero blocks the agent's tool call. Jerry having quit must never
        // do that, so this points the forwarder at a port nothing is listening on.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let files = HookFiles::write_in(temp.path()).expect("must write");
        let forwarder = files.settings_path().with_file_name(FORWARDER_NAME);

        // Bind and immediately drop, so the port is real but certainly closed.
        let dead_port = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let mut child = std::process::Command::new("/bin/sh")
            .arg(&forwarder)
            .arg("Stop")
            .env(PORT_ENV, dead_port.to_string())
            .env(TOKEN_ENV, "irrelevant")
            .env(AGENT_ENV, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        {
            use std::io::Write;
            let mut stdin = child.stdin.take().expect("stdin");
            stdin.write_all(br#"{"hook_event_name":"Stop"}"#).ok();
        }
        let output = child.wait_with_output().expect("wait");
        assert!(
            output.status.success(),
            "a dead listener must not fail the hook, got {:?}",
            output.status
        );
        assert!(
            output.stderr.is_empty(),
            "a dead listener must not print a hook error for the user, got {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn a_path_with_shell_metacharacters_is_quoted_rather_than_split() {
        assert_eq!(shell_quote("/tmp/plain"), "'/tmp/plain'");
        assert_eq!(shell_quote("/tmp/with space"), "'/tmp/with space'");
        assert_eq!(shell_quote("/tmp/$(evil)"), "'/tmp/$(evil)'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }

    #[test]
    fn a_directory_with_a_space_still_produces_a_runnable_command() {
        // The real reason `shell_quote` exists - an unquoted path here would make Claude Code run
        // a program that doesn't exist.
        let temp = tempfile::tempdir().expect("temp dir");
        let spaced = temp.path().join("a directory with spaces");
        std::fs::create_dir_all(&spaced).expect("create");
        let files = HookFiles::write_in(&spaced).expect("must write");

        let raw = std::fs::read_to_string(files.settings_path()).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .expect("a Stop command");
        assert!(command.starts_with('\''), "got {command:?}");

        if cfg!(unix) {
            // Run the generated command string through a real shell to prove it resolves.
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .stdin(std::process::Stdio::null())
                .output()
                .expect("run");
            assert!(
                output.status.success(),
                "the generated command must be runnable by a real shell: {:?}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
