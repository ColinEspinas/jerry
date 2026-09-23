//! Generating the real `--settings` file Jerry hands a spawned `claude` (GitHub issue #239,
//! phase 2; decision Q10, `docs/architecture/decisions.md` §19). Every declared hook now runs
//! `jerry hook <Event>` directly - there is no forwarder script, no port, and no token: the
//! file contains nothing launch-specific except the agent id, carried in the spawn's own
//! environment rather than in this file at all.

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

/// Env var carrying the pane's `crate::work_surface::agents::AgentId`, the same identity
/// `jerry_cli::AGENT_ENV` reads back.
pub const AGENT_ENV: &str = "JERRY_AGENT_ID";
/// Env var carrying the host socket a spawned `jerry hook` should connect to, the same one
/// `jerry_cli::SOCKET_ENV` reads back - duplicated as a literal rather than a dependency on
/// `jerry-cli`, which stays a leaf crate no other crate depends on.
pub const SOCKET_ENV: &str = "JERRY_HOST_SOCKET";

/// Name prefix of every launch directory - see [`create_private_dir`] and
/// [`sweep_stale_directories`], which are the two places that have to agree on it.
const DIRECTORY_PREFIX: &str = "jerry-hooks-";

/// The generated settings file's name inside the launch directory.
const SETTINGS_NAME: &str = "jerry-hook-settings.json";

/// The `jerry` skill (`docs/architecture/decisions.md` §21), included by file-system-relative
/// path from `jerry-cli`'s own crate directory rather than a dependency on it (`jerry-cli` stays
/// a leaf crate no other crate depends on) - the exact same bytes `jerry skill` prints.
const SKILL_MD: &str = include_str!("../../../jerry-cli/skill/SKILL.md");

/// The plugin manifest naming the skill directory - the minimal shape `claude --plugin-dir`
/// accepts (verified against a real `claude --help` on this machine and
/// <https://code.claude.com/docs/en/plugins.md>).
fn plugin_manifest_json() -> String {
    serde_json::json!({
        "name": "jerry",
        "description": "The jerry CLI: status, worktree creation, merge, and what agents Jerry is supervising.",
        "version": env!("CARGO_PKG_VERSION"),
    })
    .to_string()
}

/// The plugin-root `.mcp.json` registering `jerry mcp` - see `docs/architecture/decisions.md`
/// §22. Carries no `env`: this file is shared by every agent the launch spawns (one plugin
/// directory per launch, not per agent), so no static value here could name any one of them -
/// `JERRY_AGENT_ID`/`JERRY_HOST_SOCKET` reach the spawned server by environment inheritance
/// instead, the same way they already reach a spawned `jerry hook <event>`.
fn mcp_manifest_json(jerry_binary: &Path) -> String {
    serde_json::json!({
        "mcpServers": {
            "jerry": {
                "command": jerry_binary.to_string_lossy(),
                "args": ["mcp"],
            }
        }
    })
    .to_string()
}

/// The real on-disk files backing one Jerry launch's hook and skill injection. Removed on drop.
#[derive(Debug)]
pub struct HookFiles {
    directory: PathBuf,
    settings: PathBuf,
    /// The plugin directory to pass as `claude --plugin-dir <path>` - `directory` itself, once
    /// [`Self::fill`] has written its `.claude-plugin/plugin.json` and `SKILL.md`.
    plugin_dir: PathBuf,
}

impl HookFiles {
    /// The path to pass as `claude --settings <path>`.
    pub fn settings_path(&self) -> &Path {
        &self.settings
    }

    /// The path to pass as `claude --plugin-dir <path>`, carrying the `jerry` skill.
    pub fn plugin_dir(&self) -> &Path {
        &self.plugin_dir
    }

    /// Writes this launch's settings file, naming `jerry_binary`, plus its skill plugin
    /// directory, into a fresh private directory under `parent` (the OS temp directory in
    /// production).
    pub fn write_in(parent: &Path, jerry_binary: &Path) -> io::Result<HookFiles> {
        // Tidy away anything a previously crashed Jerry left here. Best-effort and never fatal.
        sweep_stale_directories(parent);

        let directory = create_private_dir(parent)?;
        match Self::fill(&directory, jerry_binary) {
            Ok(settings) => Ok(HookFiles {
                plugin_dir: directory.clone(),
                directory,
                settings,
            }),
            Err(err) => {
                let _ = std::fs::remove_dir_all(&directory);
                Err(err)
            }
        }
    }

    /// Writes the settings file and the skill plugin into an already-created launch `directory`,
    /// returning the settings file's own path.
    fn fill(directory: &Path, jerry_binary: &Path) -> io::Result<PathBuf> {
        let settings = directory.join(SETTINGS_NAME);
        // Not executable, and readable only by this user.
        write_private_file(&settings, settings_json(jerry_binary)?.as_bytes(), 0o600)?;

        let plugin_manifest_dir = directory.join(".claude-plugin");
        std::fs::create_dir_all(&plugin_manifest_dir)?;
        write_private_file(
            &plugin_manifest_dir.join("plugin.json"),
            plugin_manifest_json().as_bytes(),
            0o600,
        )?;
        write_private_file(&directory.join("SKILL.md"), SKILL_MD.as_bytes(), 0o600)?;
        write_private_file(
            &directory.join(".mcp.json"),
            mcp_manifest_json(jerry_binary).as_bytes(),
            0o600,
        )?;
        Ok(settings)
    }
}

impl Drop for HookFiles {
    /// Removes the whole launch directory. Best-effort: a failure here is not worth surfacing
    /// (a leftover directory in the OS temp directory holds no secret - there is no token or
    /// port here to leak, only the settings file's own read-only content), and `Drop` cannot
    /// report one anyway.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Creates this instance's launch directory: unpredictably named, owner-only from the instant it
/// exists, and refusing to reuse anything already at that path.
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

/// `CreateDirectoryW(path)`, failing if anything already exists at `path` - the Windows half of
/// [`create_private_dir`]'s three properties.
#[cfg(windows)]
fn new_dir_owner_only(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().create(path)
}

/// Writes `contents` to a newly created file with `mode`, refusing to follow or overwrite
/// anything already at `path`.
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
    // needs - harmless for a 0o600 read-only settings file (umask only ever makes it stricter),
    // kept for parity with the `0o700` case `crate::hooks::cursor_hooks_file`'s own forwarder
    // write still needs. `File::set_permissions` is `fchmod` on a descriptor already exclusively
    // owned (the file was just created with `O_EXCL`), so no path is re-resolved and there is no
    // symlink or TOCTOU window to reopen.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }

    file.write_all(contents)?;
    file.sync_all()
}

/// Removes launch directories left behind by Jerry instances that are no longer running.
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
#[cfg(unix)]
// SAFETY of the FFI call below is justified at its own call site.
#[allow(unsafe_code)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs only an existence/permission check. It has no effect
    // on the target process, and takes no pointers, so there is nothing to invalidate.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    io::Error::last_os_error().kind() == io::ErrorKind::PermissionDenied
}

/// Whether a process with this id currently exists - the Windows twin of the `kill(pid, 0)` check
/// above.
#[cfg(windows)]
// SAFETY of each FFI call below is justified at its own call site.
#[allow(unsafe_code)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: `OpenProcess` takes only scalars and returns a handle (null on failure). It borrows
    // no memory from this process, so there is nothing for it to invalidate.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // `ERROR_INVALID_PARAMETER` is Win32's real "there is no process with that id". Every
        // other failure - `ERROR_ACCESS_DENIED` above all - is reported as alive, because a wrong
        // "dead" is the only answer here that deletes anything.
        return io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32);
    }

    // SAFETY: `handle` was just returned by a successful `OpenProcess` and has not been closed, so
    // it is a valid handle this thread owns. A zero timeout makes this a poll, never a block.
    let state = unsafe { WaitForSingleObject(handle, 0) };
    // SAFETY: same handle, still owned by this function, closed exactly once and never used after.
    unsafe {
        let _ = CloseHandle(handle);
    }
    // `WAIT_OBJECT_0` - the handle became signalled - is the only outcome that is proof the process
    // has exited. `WAIT_TIMEOUT` (still running) and `WAIT_FAILED` (could not tell) both read as
    // alive; see this function's docs for what a wrong "dead" costs.
    state != WAIT_OBJECT_0
}

/// A short random, hex-encoded suffix - see [`create_private_dir`].
///
/// `pub(crate)`: `crate::hooks::cursor_hooks_file`'s own atomic-write temp file name reuses this
/// rather than a second random-suffix implementation.
pub(crate) fn random_suffix() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether Jerry can install hooks on this platform at all.
pub const fn is_supported() -> bool {
    cfg!(unix) || cfg!(windows)
}

/// Builds the real `--settings` JSON declaring every [`FORWARDED_EVENTS`] entry, each running
/// `jerry_binary hook <event>`.
fn settings_json(jerry_binary: &Path) -> io::Result<String> {
    settings_json_with(jerry_binary, hook_entry)
}

/// [`settings_json`] with the per-platform entry builder handed in explicitly.
fn settings_json_with(
    jerry_binary: &Path,
    entry: fn(&str, &str) -> io::Result<serde_json::Value>,
) -> io::Result<String> {
    let jerry_binary = jerry_binary.to_string_lossy();
    let mut hooks = serde_json::Map::new();
    for event in FORWARDED_EVENTS {
        hooks.insert(
            event.to_string(),
            serde_json::json!([{ "hooks": [entry(&jerry_binary, event)?] }]),
        );
    }
    let document = serde_json::json!({ "hooks": serde_json::Value::Object(hooks) });
    serde_json::to_string_pretty(&document)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// This platform's hook entry.
#[cfg(not(windows))]
fn hook_entry(jerry_binary: &str, event: &str) -> io::Result<serde_json::Value> {
    Ok(unix_hook_entry(jerry_binary, event))
}

/// This platform's hook entry.
#[cfg(windows)]
fn hook_entry(jerry_binary: &str, event: &str) -> io::Result<serde_json::Value> {
    windows_hook_entry(jerry_binary, event)
}

/// One `"type": "command"` entry running `jerry_binary hook <event>`, for the `sh -c` Claude
/// Code uses on macOS and Linux.
pub fn unix_hook_entry(jerry_binary: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": format!("{} hook {event}", shell_quote(jerry_binary)),
    })
}

/// One `"type": "command"` entry running `jerry_binary hook <event>` on Windows, through
/// PowerShell's call operator (`&`) - see the module docs for why the shell is pinned and why
/// the call operator, not a bare word, is required.
///
/// `powershell.exe -Command` re-joins everything after it into one script string once it has
/// arrived as a single argv element, so the whole `& <quoted> hook <event>` script is itself
/// wrapped in one pair of double quotes: that is what keeps the single-quoted path from being
/// unquoted a layer too early by whichever shell (`sh -c`, or `CreateProcess`'s own argv
/// construction) hands this command line to `powershell.exe` in the first place. Verified end to
/// end against a real `powershell.exe`, invoked through a real `sh -c` exactly as Claude Code's
/// Git Bash fallback would, with a real executable at a spaced path.
pub fn windows_hook_entry(jerry_binary: &str, event: &str) -> io::Result<serde_json::Value> {
    let quoted = powershell_quote(jerry_binary)?;
    Ok(serde_json::json!({
        "type": "command",
        // Documented Claude Code field, accepting exactly "bash" or "powershell". Without it the
        // shell on Windows is "Git Bash, or PowerShell if Git Bash isn't installed" - two quoting
        // languages, decided by what the user happens to have installed.
        "shell": "powershell",
        "command": format!(
            "powershell.exe -NoProfile -NonInteractive -Command \"& {quoted} hook {event}\""
        ),
    }))
}

/// Wraps `value` in POSIX single quotes, escaping any single quote inside it via the standard
/// `'\''` idiom. Single quotes are used rather than double because inside them the shell expands
/// nothing at all - so a path containing `$`, backticks or `\` is passed through literally.
///
/// `pub(crate)` rather than private: `crate::hooks::cursor_hooks_file` reuses this verbatim for
/// the Cursor forwarder's own `hooks.json` command string (GitHub issue #479), rather than
/// re-implementing the same escaping a second time.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The characters PowerShell's tokenizer will end a single-quoted literal on, *other* than the
/// ASCII apostrophe that opens it.
const POWERSHELL_QUOTE_DELIMITERS: [char; 4] = ['\u{2018}', '\u{2019}', '\u{201a}', '\u{201b}'];

/// Wraps `value` in PowerShell single quotes, escaping any ASCII single quote inside it by doubling
/// it - and **refusing outright** any `value` containing one of the four typographic quotes
/// PowerShell also treats as a quote delimiter ([`POWERSHELL_QUOTE_DELIMITERS`]).
pub fn powershell_quote(value: &str) -> io::Result<String> {
    if let Some(delimiter) = value
        .chars()
        .find(|character| POWERSHELL_QUOTE_DELIMITERS.contains(character))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the hook forwarder path contains U+{:04X}, which PowerShell's tokenizer treats as \
                 a closing single quote just like an ASCII one, so no quoting of this path is safe \
                 to run: {value}",
                delimiter as u32
            ),
        ));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real executable at `path` - stands in for the located `jerry` binary in a test that
    /// actually runs the generated command, so there is something real for the shell to exec.
    ///
    /// A real `#!/bin/sh` script on Unix, where any executable file will do. Windows has no
    /// script-shebang equivalent for a bare `CreateProcess`/PowerShell `&` invocation - a `.exe`
    /// path must be a genuine PE image - so there this copies a real, always-present system
    /// binary (`whoami.exe`) rather than writing text into a file merely *named* `.exe`, which
    /// Windows correctly refuses to launch at all.
    fn write_stand_in_binary(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent");
        }
        #[cfg(unix)]
        {
            write_private_file(path, b"#!/bin/sh\nexit 0\n", 0o700).expect("write stand-in");
        }
        #[cfg(windows)]
        {
            let system_root =
                std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
            let whoami = Path::new(&system_root).join("System32").join("whoami.exe");
            std::fs::copy(&whoami, path).expect("copy a real stand-in executable");
        }
    }

    fn jerry_binary_name() -> &'static str {
        if cfg!(windows) {
            "jerry.exe"
        } else {
            "jerry"
        }
    }

    #[test]
    fn the_generated_settings_declare_every_forwarded_event_against_the_jerry_binary() {
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");

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
                command_invokes_hook(command, event),
                "{event}: must invoke the jerry binary's own `hook` subcommand, got {command:?}"
            );
            assert!(
                command.contains(jerry_binary_name()),
                "{event}: must point at the real located jerry binary, got {command:?}"
            );
        }
    }

    #[test]
    fn write_in_also_writes_a_real_skill_plugin_directory_claude_can_load() {
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");

        assert_eq!(
            files.plugin_dir(),
            files.settings_path().parent().expect("parent")
        );
        let manifest_raw = std::fs::read_to_string(
            files
                .plugin_dir()
                .join(".claude-plugin")
                .join("plugin.json"),
        )
        .expect("plugin.json must exist");
        let manifest: serde_json::Value = serde_json::from_str(&manifest_raw).expect("valid JSON");
        assert_eq!(manifest["name"], serde_json::json!("jerry"));
        assert!(manifest["description"]
            .as_str()
            .is_some_and(|d| !d.is_empty()));

        let skill = std::fs::read_to_string(files.plugin_dir().join("SKILL.md"))
            .expect("SKILL.md must exist");
        assert_eq!(skill, super::SKILL_MD);
        assert!(skill.contains("jerry wt new"), "{skill}");
    }

    #[test]
    fn write_in_also_registers_jerry_mcp_in_a_real_plugin_root_mcp_json() {
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");

        let raw = std::fs::read_to_string(files.plugin_dir().join(".mcp.json"))
            .expect(".mcp.json must exist next to plugin.json/SKILL.md");
        let manifest: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let jerry_server = &manifest["mcpServers"]["jerry"];
        assert_eq!(
            jerry_server["command"],
            serde_json::json!(jerry.to_string_lossy())
        );
        assert_eq!(jerry_server["args"], serde_json::json!(["mcp"]));
    }

    #[test]
    fn the_settings_file_carries_nothing_launch_specific_but_the_jerry_path_and_the_event_name() {
        // The whole point of issue #500: no port, no token, and no forwarder script - just the
        // located `jerry` binary and the event name it is called with.
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");
        let raw = std::fs::read_to_string(files.settings_path()).expect("read");

        for forbidden in ["JERRY_HOOK_PORT", "JERRY_HOOK_TOKEN", "http://", "curl"] {
            assert!(
                !raw.contains(forbidden),
                "the settings file must never mention {forbidden:?}: {raw}"
            );
        }
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        for event in FORWARDED_EVENTS {
            let command = parsed["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{event} must declare a command"));
            assert!(command.contains(&jerry.to_string_lossy().into_owned()));
            assert!(command_invokes_hook(command, event));
        }
    }

    /// Whether `command` ends with a real `hook <event>` invocation - on Unix that is the last
    /// two words of the command string verbatim; on Windows the whole `-Command` payload is
    /// wrapped in one pair of double quotes (see [`windows_hook_entry`]'s own docs), so the
    /// closing quote follows the event name.
    fn command_invokes_hook(command: &str, event: &str) -> bool {
        command.ends_with(&format!(" hook {event}"))
            || command.ends_with(&format!(" hook {event}\""))
    }

    #[test]
    fn the_generated_settings_file_really_exists_and_is_owner_only() {
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");
        assert!(files.settings_path().is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
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
    fn a_pre_planted_symlink_cannot_capture_the_generated_settings_file() {
        // The real attack the old `create_dir_all` + chmod sequence allowed: the directory name
        // was fully predictable, `create_dir_all` returns `Ok` on an existing symlink-to-dir, and
        // `set_permissions` follows it - so a local attacker got Jerry's settings file written
        // into, and 0o700 applied to, a directory of their choosing.
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
            std::os::unix::fs::symlink(attacker.join("captured.json"), &planted_file)
                .expect("plant");
            let error = write_private_file(&planted_file, b"x", 0o600)
                .expect_err("must refuse to write through a symlink");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert!(
                !attacker.join("captured.json").exists(),
                "nothing may be written through the planted symlink"
            );
        }

        // A real run alongside that junk must still succeed, and must produce a genuine directory
        // rather than anything it adopted from the parent.
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(&parent, &jerry).expect("must still succeed");
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
        //
        // Deliberately *not* tested by setting the process umask: it is process-wide, `cargo test`
        // runs these in threads, and an earlier version of this test that did so caused spurious
        // `PermissionDenied` failures in unrelated tests running concurrently.
        //
        // Instead it asks for a mode that any ordinary umask would strip something from (0o022 and
        // 0o002 are the common defaults, and both strip bits from 0o777) and requires it back
        // exactly. Without the `fchmod` this yields 0o755 under the usual 0o022 and fails; the
        // only umask it cannot discriminate under is 0o000, where there is nothing to strip.
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
    fn the_launch_directory_is_owner_only_from_the_moment_it_exists() {
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let files = HookFiles::write_in(temp.path(), &jerry).expect("files");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = files.directory.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o700,
                "the launch directory must never be group- or world-accessible"
            );
        }
        #[cfg(windows)]
        {
            let metadata = std::fs::symlink_metadata(&files.directory).expect("stat");
            assert!(
                metadata.file_type().is_dir(),
                "the launch directory must be a real directory"
            );
            assert!(
                !metadata.file_type().is_symlink(),
                "the launch directory must never be a reparse point Jerry followed into"
            );
        }
    }

    #[test]
    fn a_dead_instance_s_directory_is_swept_but_a_live_one_s_is_left_alone() {
        // `Drop` cannot run for a SIGKILLed Jerry, so without a sweep every hard crash leaves a
        // directory behind forever. The sweep must be keyed on process liveness, not age: deleting
        // a *live* instance's directory would remove the settings file out from under its running
        // agents.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");

        let live = temp.path().join(format!("{DIRECTORY_PREFIX}1-deadbeef"));
        let dead = temp
            .path()
            .join(format!("{DIRECTORY_PREFIX}4294967294-cafebabe"));
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
        let jerry = temp.path().join(jerry_binary_name());
        let first = HookFiles::write_in(temp.path(), &jerry).expect("first");
        let second = HookFiles::write_in(temp.path(), &jerry).expect("second");
        assert!(
            first.settings_path().is_file(),
            "the first instance's files must survive the second's startup sweep"
        );
        assert!(second.settings_path().is_file());
    }

    #[test]
    fn two_instances_in_one_process_never_share_or_delete_each_other_s_files() {
        // GitHub issue #90's "New Window" puts two real `AdeApp`s in one process. With a
        // pid-only directory name they collided: the second's `write_in` deleted the first's
        // files, and the first's `Drop` then removed the directory the second was still using -
        // silently killing hook delivery for every agent in that window.
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let first = HookFiles::write_in(temp.path(), &jerry).expect("first");
        let second = HookFiles::write_in(temp.path(), &jerry).expect("second");

        assert_ne!(
            first.directory, second.directory,
            "two instances in one process must not share a launch directory"
        );
        assert!(first.settings_path().is_file());
        assert!(second.settings_path().is_file());

        let second_settings = second.settings_path().to_path_buf();
        drop(first);
        assert!(
            second_settings.is_file(),
            "closing one window must not delete the other's settings file"
        );
    }

    #[test]
    fn dropping_the_files_removes_the_whole_launch_directory() {
        let temp = tempfile::tempdir().expect("temp dir");
        let jerry = temp.path().join(jerry_binary_name());
        let settings_path;
        let directory;
        {
            let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");
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
    fn a_path_with_shell_metacharacters_is_quoted_rather_than_split() {
        assert_eq!(shell_quote("/tmp/plain"), "'/tmp/plain'");
        assert_eq!(shell_quote("/tmp/with space"), "'/tmp/with space'");
        assert_eq!(shell_quote("/tmp/$(evil)"), "'/tmp/$(evil)'");
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }

    #[test]
    fn a_directory_with_a_space_still_produces_a_runnable_command() {
        // The real reason the quoting exists - an unquoted path here would make Claude Code run
        // a program that doesn't exist. A real stand-in executable is planted at the located
        // `jerry` path so this genuinely runs the generated command end to end, not merely
        // parses it.
        let temp = tempfile::tempdir().expect("temp dir");
        let spaced = temp.path().join("a directory with spaces");
        std::fs::create_dir_all(&spaced).expect("create");
        let jerry = spaced.join(jerry_binary_name());
        write_stand_in_binary(&jerry);
        let files = HookFiles::write_in(&spaced, &jerry).expect("must write");

        let raw = std::fs::read_to_string(files.settings_path()).expect("read");
        let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
        let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .expect("a Stop command");
        if cfg!(windows) {
            assert!(command.starts_with("powershell.exe "), "got {command:?}");
            assert!(
                command.contains(&format!("\"& '{}", jerry.display())),
                "the spaced path must be quoted as one argument, got {command:?}"
            );
        } else {
            assert!(command.starts_with('\''), "got {command:?}");
        }

        #[cfg(unix)]
        {
            // Run the generated command string through a real shell to prove it resolves and
            // actually executes the stand-in binary.
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

    // ---------------------------------------------------------------------------------------
    // The Windows half.
    //
    // Everything from here to `windows_only` is deliberately *not* `#[cfg(windows)]`: it is pure
    // string generation, so it is exactly as testable on Linux as on Windows, and gating it would
    // leave the Windows quoting - the single place where getting it wrong means "hooks silently
    // never fire" - covered by no suite anybody routinely runs.
    // ---------------------------------------------------------------------------------------

    /// `powershell_quote` for a value it must accept, panicking with the refusal if it does not.
    fn quoted(value: &str) -> String {
        powershell_quote(value).unwrap_or_else(|err| panic!("{value:?} must be quotable: {err}"))
    }

    #[test]
    fn a_windows_path_with_powershell_metacharacters_is_quoted_rather_than_expanded() {
        assert_eq!(
            quoted(r"C:\Temp\plain"),
            r"'C:\Temp\plain'",
            "backslashes are literal inside single quotes in both PowerShell and bash"
        );
        assert_eq!(
            quoted(r"C:\Program Files (x86)\jerry"),
            r"'C:\Program Files (x86)\jerry'",
            "the single most common real Windows path shape: spaces and parentheses"
        );
        assert_eq!(
            quoted(r"C:\Temp\$(Get-Content secret)"),
            r"'C:\Temp\$(Get-Content secret)'",
            "a subexpression must stay literal - this is the injection case"
        );
        assert_eq!(
            quoted("C:\\Temp\\`whoami`"),
            "'C:\\Temp\\`whoami`'",
            "a backtick is PowerShell's escape character *outside* single quotes only"
        );
        assert_eq!(
            quoted(r"C:\Users\O'Brien\jerry"),
            r"'C:\Users\O''Brien\jerry'",
            "an apostrophe is doubled - PowerShell's own escape, see `powershell_quote`'s docs"
        );
        assert_eq!(
            quoted(r"C:\Temp\a&b;c|d"),
            r"'C:\Temp\a&b;c|d'",
            "command separators must stay literal"
        );
    }

    #[test]
    fn a_path_containing_any_of_powershells_four_other_quote_characters_is_refused_outright() {
        // The vulnerability this whole `Result` exists for. PowerShell's tokenizer does not have
        // one single-quote character, it has five: `IsSingleQuote` also answers true for U+2018,
        // U+2019, U+201A and U+201B, and `ScanStringLiteral` ends the literal on *any* of them,
        // symmetrically - so a literal opened with an ASCII `'` is closed just as happily by a `'`.
        let hostile = "C:\\Temp\\x\u{2019}; Start-Process calc.exe; \u{2018}";
        for delimiter in POWERSHELL_QUOTE_DELIMITERS {
            let path = format!("C:\\Temp\\x{delimiter}; Start-Process calc.exe; {delimiter}");
            let error = powershell_quote(&path)
                .expect_err("a path that can close the literal must be refused, never quoted");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                error
                    .to_string()
                    .contains(&format!("U+{:04X}", delimiter as u32)),
                "the refusal must name the character, or nobody can diagnose it: {error}"
            );
            assert!(
                windows_hook_entry(&path, "Stop").is_err(),
                "the hook entry must not be built at all for {delimiter:?}"
            );
        }
        assert!(powershell_quote(hostile).is_err());

        // The realistic, non-adversarial case, which is the reason this is a bug and not merely a
        // hardening exercise. `O'Brien` with a typographic apostrophe is a legal Windows profile
        // directory - the character is not reserved, and word processors, browsers and chat
        // clients autocorrect a straight quote into one.
        let obrien = "C:\\Users\\O\u{2019}Brien\\AppData\\Local\\Temp\\jerry.exe";
        assert!(
            powershell_quote(obrien).is_err(),
            "a merely unlucky path must be refused too, so hooks are skipped rather than broken"
        );

        // Positive controls: everything that is *not* one of the five must still be quoted, and an
        // ordinary ASCII apostrophe is still handled by doubling rather than by refusing.
        assert_eq!(
            quoted(r"C:\Users\O'Brien\AppData\Local\Temp\jerry.exe"),
            r"'C:\Users\O''Brien\AppData\Local\Temp\jerry.exe'",
            "the straight apostrophe is escapable, and must not be caught by the refusal"
        );
        for inert in [
            '\u{201c}', '\u{201d}', '\u{201e}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{00a0}',
            '\u{0085}', '"', '`', '$',
        ] {
            let path = format!("C:\\Temp\\x{inert}y\\jerry.exe");
            assert_eq!(
                quoted(&path),
                format!("'{path}'"),
                "{inert:?} cannot end a single-quoted literal and must not be refused"
            );
        }
    }

    #[test]
    fn a_refusal_to_quote_skips_hook_injection_entirely_rather_than_writing_a_broken_file() {
        // The whole point of `powershell_quote` returning a `Result`: the refusal has to travel
        // all the way to `HookFiles::write_in`'s `io::Result`, because that is what
        // `crate::hooks::HookRuntime::start` already turns into "log it and return `None`", i.e.
        // into the same graceful fallback an unwritable temp directory takes.
        let refused = Path::new("/tmp/O\u{2019}Brien/jerry.exe");
        let error = settings_json_with(refused, windows_hook_entry)
            .expect_err("an unquotable jerry path must fail the whole settings file");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("U+2019"),
            "the message reaching HookRuntime::start's log line must say what went wrong: {error}"
        );

        // And the ordinary path must still build a complete file, so the refusal cannot be a
        // blanket "Windows settings never generate".
        let fine = Path::new(r"C:\Temp\jerry.exe");
        let json = settings_json_with(fine, windows_hook_entry).expect("must still be generated");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            parsed["hooks"].as_object().map(serde_json::Map::len),
            Some(FORWARDED_EVENTS.len())
        );
    }

    #[test]
    fn a_hostile_path_cannot_break_out_of_the_generated_windows_command() {
        // The adversarial case. If a temp path could end the quoted region early, everything
        // after it becomes live PowerShell running as the user, fired by Claude Code on every
        // tool call. The property that makes that impossible for an ASCII apostrophe is that a
        // single-quoted PowerShell string has no internal escape sequence at all: only an *odd*
        // run of quotes can end it, and doubling guarantees every run is even.
        let hostile = r"C:\Temp\x'; Start-Process calc.exe; '";
        let entry = windows_hook_entry(hostile, "Stop").expect("an ASCII apostrophe is quotable");
        let command = entry["command"].as_str().expect("a command string");

        let quoted = quoted(hostile);
        assert!(
            command.contains(&quoted),
            "the whole path must appear as one quoted literal, got {command:?}"
        );
        assert!(
            command.starts_with("powershell.exe -NoProfile -NonInteractive -Command \"& "),
            "the fixed interpreter/call-operator preamble must precede the quoted path, got \
             {command:?}"
        );
        assert!(
            command.ends_with(" hook Stop\""),
            "nothing but the event name may follow the quoted path, got {command:?}"
        );
        // And the quoting must be balanced: an odd number of quotes would mean the literal was
        // left open, which is exactly how a break-out looks.
        assert_eq!(
            command.matches('\'').count() % 2,
            0,
            "unbalanced quoting in {command:?}"
        );
    }

    #[test]
    fn the_windows_hook_entry_pins_the_shell_and_passes_the_event() {
        // Without `shell`, Claude Code's own docs say the Windows shell is "Git Bash, or PowerShell
        // if Git Bash isn't installed" - two quoting languages chosen by what the user happens to
        // have installed. Pinning it is what makes `powershell_quote` the *right* quoter rather
        // than a coin flip.
        for event in FORWARDED_EVENTS {
            let entry = windows_hook_entry(r"C:\Temp\jerry.exe", event)
                .expect("an ordinary Windows path must be quotable");
            assert_eq!(entry["type"], "command");
            assert_eq!(
                entry["shell"], "powershell",
                "{event}: the shell must be pinned, not inferred"
            );
            let command = entry["command"].as_str().expect("a command");
            assert!(
                command.ends_with(&format!(" hook {event}\"")),
                "{event}: the event name must reach jerry's own hook subcommand, got {command:?}"
            );
            assert!(
                command.contains("-NoProfile") && command.contains("-NonInteractive"),
                "{event}: the user's PowerShell profile must not run on every hook, and it must \
                 never prompt - got {command:?}"
            );
        }
    }

    #[test]
    fn the_unix_hook_entry_is_untouched_by_the_windows_one_existing() {
        let entry = unix_hook_entry("/usr/local/bin/jerry", "Stop");
        assert_eq!(entry["type"], "command");
        assert!(
            entry.get("shell").is_none(),
            "Unix must keep declaring no shell - `sh -c` is already Claude Code's default there"
        );
        assert_eq!(entry["command"], "'/usr/local/bin/jerry' hook Stop");
    }

    #[test]
    fn hook_injection_is_supported_on_every_platform_with_a_jerry_binary() {
        assert_eq!(is_supported(), cfg!(unix) || cfg!(windows));
    }

    /// The tests that genuinely need a Windows kernel: a real `powershell.exe` running the real
    /// generated command against a real stand-in `jerry.exe`.
    #[cfg(windows)]
    mod windows_only {
        use super::*;

        /// The real generated `Stop` command, read back out of the real generated settings file
        /// rather than reconstructed - so this exercises the string Claude Code would actually
        /// run, wrapped in the shell `"shell": "powershell"` asks Claude Code for.
        fn generated_stop_command(files: &HookFiles) -> std::process::Command {
            let raw = std::fs::read_to_string(files.settings_path()).expect("read settings");
            let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
            let command = parsed["hooks"]["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .expect("a Stop command")
                .to_owned();

            let mut process = std::process::Command::new("powershell.exe");
            process
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(&command)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            process
        }

        #[test]
        fn the_real_generated_command_runs_the_real_stand_in_jerry_binary() {
            let temp = tempfile::tempdir().expect("temp dir");
            let jerry = temp.path().join("jerry.exe");
            write_stand_in_binary(&jerry);
            let files = HookFiles::write_in(temp.path(), &jerry).expect("must write");

            let output = generated_stop_command(&files)
                .output()
                .expect("the generated command must run");
            // The stand-in (`whoami.exe`) does not accept `hook`/`Stop` as arguments and exits
            // non-zero complaining about them - which is exactly the proof this test needs: a
            // real process really started at the exact quoted path (an error PowerShell itself
            // raises, e.g. "is not recognized" or "not a valid Win32 application", would mean the
            // quoting broke *before* a process ever started).
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.code().is_some(),
                "the shell must really have started the stand-in binary: {:?}: {stderr}",
                output.status
            );
            assert!(
                !stderr.contains("ApplicationFailedException")
                    && !stderr.contains("ObjectNotFound"),
                "a PowerShell-level launch failure means the quoting broke, not the stand-in's \
                 own business logic: {stderr}"
            );
        }

        #[test]
        fn hook_injection_is_skipped_when_the_launch_path_cannot_be_quoted() {
            // A temp directory whose name contains a typographic apostrophe (legal on Windows,
            // and the realistic `C:\Users\O'Brien` case) must make the whole thing fail, so
            // `HookRuntime::start` logs it and falls back, instead of a settings file being
            // written that names a command PowerShell cannot parse.
            let temp = tempfile::tempdir().expect("temp dir");
            let parent = temp.path().join("O\u{2019}Brien");
            std::fs::create_dir_all(&parent).expect("create");
            let jerry = parent.join("jerry.exe");
            write_stand_in_binary(&jerry);
            let error = HookFiles::write_in(&parent, &jerry)
                .expect_err("an unquotable jerry path must not produce a settings file");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }

        #[test]
        fn a_live_windows_process_reads_as_alive_and_a_freed_pid_does_not() {
            // `process_is_alive` decides whether `sweep_stale_directories` deletes a directory, so
            // a wrong "dead" removes a *running* Jerry's settings file out from under its agents.
            let mut child = std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 30",
                ])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn a real child");
            let pid = child.id();
            assert!(
                process_is_alive(pid),
                "a process that is genuinely running must read as alive"
            );
            assert!(
                process_is_alive(std::process::id()),
                "this very process must read as alive"
            );

            child.kill().expect("kill");
            child.wait().expect("reap");
            drop(child);

            assert!(
                !process_is_alive(u32::MAX - 1),
                "a pid that cannot exist must read as dead, or nothing is ever swept"
            );
        }

        #[test]
        fn a_dead_instance_s_directory_is_swept_but_a_live_one_s_is_left_alone() {
            let temp = tempfile::tempdir().expect("temp dir");
            let live = temp
                .path()
                .join(format!("{DIRECTORY_PREFIX}{}-deadbeef", std::process::id()));
            let dead = temp
                .path()
                .join(format!("{DIRECTORY_PREFIX}{}-cafebabe", u32::MAX - 1));
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
        fn the_launch_directory_cannot_be_captured_by_something_already_at_the_path() {
            let temp = tempfile::tempdir().expect("temp dir");
            let taken = temp.path().join("already-here");
            std::fs::create_dir_all(&taken).expect("create");
            let error = new_dir_owner_only(&taken).expect_err("must refuse an existing path");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

            let planted = temp.path().join("planted-file");
            std::fs::write(&planted, b"theirs").expect("write");
            let error = write_private_file(&planted, b"ours", 0o600)
                .expect_err("must refuse an existing file");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(
                std::fs::read(&planted).expect("read"),
                b"theirs",
                "an existing file must never be written through"
            );
        }
    }
}
