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

/// Name prefix of every launch directory - see [`create_private_dir`] and
/// [`sweep_stale_directories`], which are the two places that have to agree on it.
const DIRECTORY_PREFIX: &str = "jerry-hooks-";

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
        // Tidy away anything a previously crashed Jerry left here. Best-effort and never fatal.
        sweep_stale_directories(parent);

        let directory = create_private_dir(parent)?;

        let forwarder = directory.join(FORWARDER_NAME);
        // Executable, but only by this user.
        write_private_file(&forwarder, FORWARDER_SCRIPT.as_bytes(), 0o700)?;

        let settings = directory.join(SETTINGS_NAME);
        // Not executable, and readable only by this user.
        write_private_file(&settings, settings_json(&forwarder)?.as_bytes(), 0o600)?;

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

/// Creates this instance's launch directory: unpredictably named, owner-only from the instant it
/// exists, and refusing to reuse anything already at that path.
///
/// All three properties are load-bearing, and an earlier version of this had none of them. It
/// built a fully predictable name (`jerry-hooks-<pid>-<counter>`) under the world-writable OS temp
/// directory, called `create_dir_all`, and only *then* chmod'ed to `0o700`. That is two real bugs:
///
/// - **A permissions window.** `mkdir` applies `0o777 & !umask`, so under a permissive umask
///   (`002`, `000` - both real in the wild) the directory was group- or world-writable for the
///   window between creation and the chmod.
/// - **A symlink attack, which is the serious one.** `create_dir_all` on a path that is already a
///   symlink to a directory returns `Ok`, and `set_permissions` follows symlinks. So a local
///   attacker who pre-created `<temp>/jerry-hooks-<pid>-0` as a symlink - trivial, since every
///   component of the name was predictable - got the forwarder script written into, and `0o700`
///   applied to, a directory of their choosing. Verified empirically before fixing.
///
/// The fix is to make creation itself atomic and exclusive rather than to repair the state after:
/// [`std::fs::DirBuilder`] with an explicit `mode` applies the permissions in the `mkdir` syscall
/// (umask can only *remove* bits, so the result is never more permissive than `0o700`), and
/// `create` - unlike `recursive(true)` - fails with `AlreadyExists` if anything is already at the
/// path, symlink included. A random suffix then removes the predictability that made pre-creation
/// worth attempting at all, and the retry loop covers the (vanishing) chance of a collision.
///
/// The pid stays in the name so [`sweep_stale_directories`] can still tell a dead instance's
/// leftovers from a live instance's working directory.
fn create_private_dir(parent: &Path) -> io::Result<PathBuf> {
    std::fs::create_dir_all(parent)?;
    let mut last_error = None;
    for _ in 0..16 {
        let directory = parent.join(format!(
            "{DIRECTORY_PREFIX}{}-{}",
            std::process::id(),
            random_suffix()
        ));
        match new_dir_owner_only(&directory) {
            Ok(()) => return Ok(directory),
            Err(err) => last_error = Some(err),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("could not create a private hook directory")))
}

/// `mkdir(path, 0o700)`, failing if anything already exists at `path`.
#[cfg(unix)]
fn new_dir_owner_only(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// Windows has no `mode`; hook injection is disabled there anyway ([`is_supported`]), so this
/// exists only to keep the module compiling.
#[cfg(not(unix))]
fn new_dir_owner_only(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().create(path)
}

/// Writes `contents` to a newly created file with `mode`, refusing to follow or overwrite
/// anything already at `path`.
///
/// `create_new` is `O_EXCL | O_CREAT`, which fails on an existing file *and* on a symlink rather
/// than writing through it, and the mode is applied by `open` itself rather than by a later
/// chmod - the same "atomic, not repaired afterwards" reasoning as [`create_private_dir`]. The
/// containing directory is already `0o700` and unpredictably named by the time this runs, so this
/// is defence in depth rather than the primary barrier.
fn write_private_file(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path)?;

    // `open`'s mode is masked by the process umask, which can *remove* bits the file genuinely
    // needs. That is harmless for the 0o600 settings file (umask only ever makes it stricter),
    // but not for the 0o700 forwarder: a umask with owner bits set - 0o100 and 0o700 are unusual
    // but entirely legal - creates it non-executable, Claude Code's shell invocation exits 126,
    // and hooks then silently never fire, with the rail quietly falling back to the Phase 1
    // heuristics and no error anywhere. Verified: under umask 0o100, `open(.., 0o700)` yields
    // 0o600, and this `fchmod` restores 0o700.
    //
    // Safe to do *after* creation, unlike the chmod-after-`create_dir_all` this module used to
    // do: `File::set_permissions` is `fchmod` on a descriptor already exclusively owned (the file
    // was just created with `O_EXCL`), so no path is re-resolved and there is no symlink or
    // TOCTOU window to reopen - the C2 race came from re-resolving a *path*, not from ordering.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }

    file.write_all(contents)?;
    file.sync_all()
}

/// Removes launch directories left behind by Jerry instances that are no longer running.
///
/// [`HookFiles::drop`] handles the normal case, but `Drop` does not run for a `SIGKILL`, a hard
/// crash, or an `abort` - so without this, every abnormal exit leaves a small directory in the OS
/// temp directory forever. Called once per [`HookFiles::write_in`], which is the only moment
/// Jerry is guaranteed to be looking at this directory anyway.
///
/// Liveness is decided by the pid embedded in the name, *not* by age. An age-based sweep - the
/// convention this codebase uses for its `*.tmp` siblings - would be wrong here: a Jerry left open
/// for a week is entirely normal, and deleting its forwarder script out from under its running
/// agents would silently kill their hooks, which is precisely the bug the per-instance directory
/// naming exists to prevent.
///
/// Everything is best-effort. A directory whose name doesn't parse, or that belongs to a live
/// process, or that refuses to delete (another user's, on a shared temp directory) is simply left
/// alone - this is tidying, and it must never be able to fail a launch.
fn sweep_stale_directories(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let own_pid = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(rest) = name.strip_prefix(DIRECTORY_PREFIX) else {
            continue;
        };
        // `<pid>-<random>`; anything else was not written by this code.
        let Some((pid, _)) = rest.split_once('-') else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == own_pid || process_is_alive(pid) {
            continue;
        }
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

/// Whether a process with this id currently exists.
///
/// `kill(pid, 0)` is the portable POSIX existence check - it sends no signal and only reports
/// whether the process exists and could be signalled. `EPERM` counts as alive: the process is
/// real, it simply belongs to another user, which is a case that genuinely occurs on a shared
/// `/tmp` and must not be read as "dead, delete its files".
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs only an existence/permission check. It has no effect
    // on the target process, and takes no pointers, so there is nothing to invalidate.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
}

/// Windows: hook injection is disabled ([`is_supported`]), so nothing is ever written to sweep.
/// Reporting "alive" is the conservative answer - it deletes nothing.
#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

/// A short random, hex-encoded suffix - see [`create_private_dir`].
fn random_suffix() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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
    fn a_pre_planted_symlink_cannot_capture_the_generated_files() {
        // The real attack the old `create_dir_all` + chmod sequence allowed: the directory name
        // was fully predictable, `create_dir_all` returns `Ok` on an existing symlink-to-dir, and
        // `set_permissions` follows it - so a local attacker got Jerry's forwarder script written
        // into, and 0o700 applied to, a directory of their choosing.
        //
        // Asserted directly against the creation primitives rather than by planting symlinks at
        // the names the *old* scheme would have picked: under the random suffix those names can
        // never be chosen, so such a test would pass without exercising anything. What actually
        // has to hold is that creation is exclusive - an attacker who guesses the name anyway
        // still gets a hard error instead of a captured directory - and that is what this pins.
        // Both assertions below fail against the old `create_dir_all`/`fs::write`, which return
        // `Ok` here and write straight through the symlink.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let parent = temp.path().join("parent");
        let attacker = temp.path().join("attacker-owned");
        std::fs::create_dir_all(&parent).expect("parent");
        std::fs::create_dir_all(&attacker).expect("attacker dir");

        #[cfg(unix)]
        {
            let guessed = parent.join("guessed-name");
            std::os::unix::fs::symlink(&attacker, &guessed).expect("plant");
            let error = new_dir_owner_only(&guessed).expect_err("must refuse an existing path");
            assert_eq!(
                error.kind(),
                io::ErrorKind::AlreadyExists,
                "directory creation must be exclusive, never reuse-what's-there"
            );

            let planted_file = parent.join("planted-file");
            std::os::unix::fs::symlink(attacker.join("captured.sh"), &planted_file).expect("plant");
            let error = write_private_file(&planted_file, b"x", 0o600)
                .expect_err("must refuse to write through a symlink");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert!(
                !attacker.join("captured.sh").exists(),
                "nothing may be written through the planted symlink"
            );
        }

        // A real run alongside that junk must still succeed, and must produce a genuine directory
        // rather than anything it adopted from the parent.
        let files = HookFiles::write_in(&parent).expect("must still succeed");
        let metadata = std::fs::symlink_metadata(&files.directory).expect("stat");
        assert!(
            metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
            "the launch directory must be a real directory Jerry created itself"
        );
        let captured: Vec<_> = std::fs::read_dir(&attacker)
            .expect("read attacker dir")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            captured.is_empty(),
            "nothing may have reached the attacker's directory: {captured:?}"
        );
    }

    #[test]
    fn a_written_file_gets_exactly_the_mode_asked_for_not_the_umask_s_opinion_of_it() {
        // `open`'s mode argument is masked by the umask, which can *remove* bits the file needs.
        // For the 0o700 forwarder that is a silent failure with real consequences: under a umask
        // with owner bits set (0o100 is unusual but entirely legal) it would be created
        // non-executable, Claude Code's shell invocation would exit 126, and hooks would simply
        // never fire - the rail falling back to the Phase 1 heuristics with nothing reporting an
        // error anywhere. `write_private_file` therefore `fchmod`s the descriptor it already owns.
        //
        // Deliberately *not* tested by setting the process umask: it is process-wide, `cargo test`
        // runs these in threads, and an earlier version of this test that did so caused spurious
        // `PermissionDenied` failures in unrelated tests running concurrently.
        //
        // Instead it asks for a mode that any ordinary umask would strip something from (0o022 and
        // 0o002 are the common defaults, and both strip bits from 0o777) and requires it back
        // exactly. Without the `fchmod` this yields 0o755 under the usual 0o022 and fails; the
        // only umask it cannot discriminate under is 0o000, where there is nothing to strip.
        if !cfg!(unix) {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let temp = tempfile::tempdir().expect("temp dir");
            let path = temp.path().join("mode-probe");
            write_private_file(&path, b"x", 0o777).expect("write");
            let mode = path.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o777,
                "the requested mode must survive the umask, got {mode:o}"
            );
        }
    }

    #[test]
    fn the_generated_forwarder_is_directly_executable() {
        // The property the 0o700 mode exists for, asserted end-to-end rather than as bits: if
        // this ever regresses, Claude Code's invocation of the hook exits 126 and every hook
        // silently stops firing.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let files = HookFiles::write_in(temp.path()).expect("files");
        let forwarder = files.settings_path().with_file_name(FORWARDER_NAME);
        let output = std::process::Command::new(&forwarder)
            .arg("Stop")
            .env_remove(PORT_ENV)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("the forwarder must be directly executable, not merely readable");
        assert!(output.status.success());
    }

    #[test]
    fn the_launch_directory_is_owner_only_from_the_moment_it_exists() {
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let files = HookFiles::write_in(temp.path()).expect("files");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = files.directory.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "the launch directory must never be group- or world-accessible"
            );
        }
    }

    #[test]
    fn a_dead_instance_s_directory_is_swept_but_a_live_one_s_is_left_alone() {
        // `Drop` cannot run for a SIGKILLed Jerry, so without a sweep every hard crash leaves a
        // directory behind forever. The sweep must be keyed on process liveness, not age: deleting
        // a *live* instance's directory would remove the forwarder script out from under its
        // running agents.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");

        // pid 1 always exists; a "dead" pid that no longer does.
        let live = temp.path().join(format!("{DIRECTORY_PREFIX}1-deadbeef"));
        let dead = temp
            .path()
            .join(format!("{DIRECTORY_PREFIX}4294967294-cafebabe"));
        // Something that simply isn't ours.
        let unrelated = temp.path().join("someone-elses-directory");
        for path in [&live, &dead, &unrelated] {
            std::fs::create_dir_all(path).expect("create");
            std::fs::write(path.join("marker"), b"x").expect("write");
        }

        sweep_stale_directories(temp.path());

        assert!(
            live.exists(),
            "a live instance's directory must be left alone"
        );
        assert!(!dead.exists(), "a dead instance's directory must be swept");
        assert!(
            unrelated.exists(),
            "unrelated entries must never be touched"
        );
    }

    #[test]
    fn the_sweep_never_removes_this_instance_s_own_directory() {
        // The sweep runs from inside `write_in`, so getting this wrong would delete the files of
        // the very instance that just asked for them - and of every sibling window in this process.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let first = HookFiles::write_in(temp.path()).expect("first");
        let second = HookFiles::write_in(temp.path()).expect("second");
        assert!(
            first.settings_path().is_file(),
            "the first instance's files must survive the second's startup sweep"
        );
        assert!(second.settings_path().is_file());
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
