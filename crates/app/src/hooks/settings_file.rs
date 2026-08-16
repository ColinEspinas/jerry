//! Generating the real `--settings` file and forwarder script Jerry hands a spawned `claude`
//! (GitHub issue #239, phase 2).

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

/// The POSIX forwarder script's file name inside the launch directory.
pub const UNIX_FORWARDER_NAME: &str = "jerry-hook-forwarder.sh";
/// The PowerShell forwarder script's file name inside the launch directory.
pub const WINDOWS_FORWARDER_NAME: &str = "jerry-hook-forwarder.ps1";

/// The forwarder script's file name inside the launch directory, on *this* platform.
#[cfg(not(windows))]
pub const FORWARDER_NAME: &str = UNIX_FORWARDER_NAME;
/// The forwarder script's file name inside the launch directory, on *this* platform.
#[cfg(windows)]
pub const FORWARDER_NAME: &str = WINDOWS_FORWARDER_NAME;

/// The generated settings file's name inside the launch directory.
const SETTINGS_NAME: &str = "jerry-hook-settings.json";

/// The POSIX `sh` forwarder - see the module docs for why it is shaped exactly like this.
pub const UNIX_FORWARDER_SCRIPT: &str = r#"#!/bin/sh
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

/// The Windows PowerShell forwarder - the exact same contract as [`UNIX_FORWARDER_SCRIPT`] (no-op
/// without the `JERRY_*` environment, POST stdin verbatim, parse nothing, always exit 0), written
/// in the one language the module docs explain Jerry pins Claude Code's hook shell to.
pub const WINDOWS_FORWARDER_SCRIPT: &str = r#"# Written by Jerry (github.com/ColinEspinas/jerry) to forward Claude Code hook payloads to the
# Jerry instance that spawned this agent. Safe to run anywhere: without the JERRY_* environment
# variables Jerry injects on the panes it spawns, this exits immediately having done nothing.
param([string] $JerryHookEvent = '')

# Nothing in here may ever fail the hook: a non-zero exit blocks the agent's tool call (exit 2),
# and any other non-zero code shows the user a hook error. Every path out of this file exits 0.
$ErrorActionPreference = 'SilentlyContinue'
$ProgressPreference = 'SilentlyContinue'

if (-not $env:JERRY_HOOK_PORT) { exit 0 }
if (-not $env:JERRY_HOOK_TOKEN) { exit 0 }
if (-not $env:JERRY_AGENT_ID) { exit 0 }
if (-not $JerryHookEvent) { exit 0 }

# One element of curl's argument vector, spelled the way CommandLineToArgvW parses it back:
# wrapped in double quotes, with an embedded quote written \" and every run of backslashes that
# immediately precedes a quote (the closing one included) doubled. This is a CreateProcess command
# line and never a shell one - nothing expands & | ; $ or a backtick at any point - so the quote
# and the backslash run before it are the only characters here with any meaning at all.
#
# Spelled out rather than handed over as a real vector because ProcessStartInfo.ArgumentList, which
# would do exactly this inside the framework, does not exist on .NET Framework - and Windows
# PowerShell 5.1, the powershell.exe every Windows still ships, runs on .NET Framework.
function ConvertTo-JerryArgument {
    param([string] $JerryValue)
    $JerryEscaped = ''
    $JerrySlashes = 0
    foreach ($JerryChar in $JerryValue.ToCharArray()) {
        if ($JerryChar -eq '\') { $JerrySlashes++; continue }
        if ($JerryChar -eq '"') {
            $JerryEscaped += '\' * (2 * $JerrySlashes + 1) + '"'
            $JerrySlashes = 0
            continue
        }
        if ($JerrySlashes -gt 0) { $JerryEscaped += '\' * $JerrySlashes; $JerrySlashes = 0 }
        $JerryEscaped += $JerryChar
    }
    return '"' + $JerryEscaped + ('\' * (2 * $JerrySlashes)) + '"'
}

$JerryCurlProcess = $null
try {
    # curl.exe ships in System32 on Windows 10 1803+ and Windows 11. Prefer that exact path over
    # a PATH lookup, then fall back to PATH, then give up quietly - an older Windows without it
    # must cost the agent nothing at all.
    $JerryCurl = Join-Path -Path "$env:SystemRoot" -ChildPath 'System32\curl.exe'
    if (-not (Test-Path -LiteralPath $JerryCurl -PathType Leaf)) {
        $JerryFound = @(Get-Command -Name 'curl.exe' -CommandType Application -ErrorAction SilentlyContinue)
        if ($JerryFound.Count -eq 0) { exit 0 }
        $JerryCurl = $JerryFound[0].Source
    }

    # Every one of these is a separate element of curl's argument vector, so a value containing a
    # space, a quote or an ampersand is data and can never become another curl option.
    $JerryArgs = @(
        '--silent',
        '--max-time', '5',
        '--request', 'POST',
        '--header', "Authorization: Bearer $($env:JERRY_HOOK_TOKEN)",
        '--header', 'Content-Type: application/json',
        '--data-binary', '@-',
        "http://127.0.0.1:$($env:JERRY_HOOK_PORT)/hook?event=$JerryHookEvent&agent=$($env:JERRY_AGENT_ID)"
    )

    $JerryStart = New-Object System.Diagnostics.ProcessStartInfo
    $JerryStart.FileName = $JerryCurl
    $JerryStart.Arguments = (($JerryArgs | ForEach-Object { ConvertTo-JerryArgument $_ }) -join ' ')
    $JerryStart.UseShellExecute = $false
    $JerryStart.CreateNoWindow = $true
    $JerryStart.RedirectStandardInput = $true
    $JerryStart.RedirectStandardOutput = $true
    $JerryStart.RedirectStandardError = $true
    $JerryCurlProcess = [System.Diagnostics.Process]::Start($JerryStart)

    # Drained into the bit bucket, asynchronously and before a single request byte is written, so
    # neither output pipe can fill and deadlock curl while this script is still feeding it the
    # body. curl's stdout on some events would otherwise be fed back to the model as context, and
    # its stderr would be shown to the user as a hook error.
    [void] $JerryCurlProcess.StandardOutput.BaseStream.CopyToAsync([System.IO.Stream]::Null)
    [void] $JerryCurlProcess.StandardError.BaseStream.CopyToAsync([System.IO.Stream]::Null)

    # The payload goes from this process's stdin straight into curl's, and is never written to disk
    # at any point. `.BaseStream` is the raw pipe underneath the StreamWriter, so no encoder ever
    # sees a byte of it - see the module docs for why every string-shaped route through PowerShell
    # corrupts a non-ASCII payload on Windows.
    $JerryStdin = [Console]::OpenStandardInput()
    try { $JerryStdin.CopyTo($JerryCurlProcess.StandardInput.BaseStream) }
    finally { $JerryCurlProcess.StandardInput.Close() }

    # curl bounds its own work with --max-time 5; this bounds the whole child, so a curl that
    # somehow never exits can neither hold the agent's tool call open nor be left as an orphan.
    if (-not $JerryCurlProcess.WaitForExit(10000)) { $JerryCurlProcess.Kill() }
} catch {
    # Deliberately swallowed. A dead listener, a curl that will not start, or a stdin that closes
    # mid-copy must all cost exactly nothing.
} finally {
    if ($JerryCurlProcess) { $JerryCurlProcess.Dispose() }
}

exit 0
"#;

/// The forwarder script this platform writes.
#[cfg(not(windows))]
const FORWARDER_SCRIPT: &str = UNIX_FORWARDER_SCRIPT;
/// The forwarder script this platform writes.
#[cfg(windows)]
const FORWARDER_SCRIPT: &str = WINDOWS_FORWARDER_SCRIPT;

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
    pub fn write_in(parent: &Path) -> io::Result<HookFiles> {
        // Tidy away anything a previously crashed Jerry left here. Best-effort and never fatal.
        sweep_stale_directories(parent);

        let directory = create_private_dir(parent)?;
        match Self::fill(&directory) {
            Ok(settings) => Ok(HookFiles {
                directory,
                settings,
            }),
            Err(err) => {
                let _ = std::fs::remove_dir_all(&directory);
                Err(err)
            }
        }
    }

    /// Writes both generated files into an already-created launch `directory`, returning the
    /// settings file's path. Split out of [`Self::write_in`] purely so every failure between the
    /// two has one place to be cleaned up from.
    fn fill(directory: &Path) -> io::Result<PathBuf> {
        let forwarder = directory.join(FORWARDER_NAME);
        // Executable, but only by this user. (Windows has no execute bit and does not need one -
        // the script is run as an argument to `powershell.exe -File`, never by exec'ing the file
        // itself; see [`write_private_file`].)
        write_private_file(&forwarder, FORWARDER_SCRIPT.as_bytes(), 0o700)?;

        let settings = directory.join(SETTINGS_NAME);
        // Not executable, and readable only by this user.
        write_private_file(&settings, settings_json(&forwarder)?.as_bytes(), 0o600)?;
        Ok(settings)
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
/// [`create_private_dir`]'s three properties, and a real code path now that [`is_supported`] is
/// true here.
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
fn random_suffix() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether Jerry can install hooks on this platform at all.
pub const fn is_supported() -> bool {
    cfg!(unix) || cfg!(windows)
}

/// Builds the real `--settings` JSON declaring every [`FORWARDED_EVENTS`] entry against
/// `forwarder`.
fn settings_json(forwarder: &Path) -> io::Result<String> {
    settings_json_with(forwarder, hook_entry)
}

/// [`settings_json`] with the per-platform entry builder handed in explicitly.
fn settings_json_with(
    forwarder: &Path,
    entry: fn(&str, &str) -> io::Result<serde_json::Value>,
) -> io::Result<String> {
    let forwarder = forwarder.to_string_lossy();
    let mut hooks = serde_json::Map::new();
    for event in FORWARDED_EVENTS {
        hooks.insert(
            event.to_string(),
            serde_json::json!([{ "hooks": [entry(&forwarder, event)?] }]),
        );
    }
    let document = serde_json::json!({ "hooks": serde_json::Value::Object(hooks) });
    serde_json::to_string_pretty(&document)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// This platform's hook entry.
#[cfg(not(windows))]
fn hook_entry(forwarder: &str, event: &str) -> io::Result<serde_json::Value> {
    Ok(unix_hook_entry(forwarder, event))
}

/// This platform's hook entry.
#[cfg(windows)]
fn hook_entry(forwarder: &str, event: &str) -> io::Result<serde_json::Value> {
    windows_hook_entry(forwarder, event)
}

/// One `"type": "command"` entry running the POSIX forwarder, for the `sh -c` Claude Code uses on
/// macOS and Linux.
pub fn unix_hook_entry(forwarder: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": format!("{} {event}", shell_quote(forwarder)),
    })
}

/// One `"type": "command"` entry running the PowerShell forwarder on Windows - see the module docs
/// for why the shell is pinned, why the invocation goes through a second `powershell.exe`, and why
/// the resulting string is deliberately also valid `bash`.
pub fn windows_hook_entry(forwarder: &str, event: &str) -> io::Result<serde_json::Value> {
    let quoted = powershell_quote(forwarder)?;
    Ok(serde_json::json!({
        "type": "command",
        // Documented Claude Code field, accepting exactly "bash" or "powershell". Without it the
        // shell on Windows is "Git Bash, or PowerShell if Git Bash isn't installed" - two quoting
        // languages, decided by what the user happens to have installed.
        "shell": "powershell",
        "command": format!(
            "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {quoted} {event}"
        ),
    }))
}

/// Wraps `value` in POSIX single quotes, escaping any single quote inside it via the standard
/// `'\''` idiom. Single quotes are used rather than double because inside them the shell expands
/// nothing at all - so a path containing `$`, backticks or `\` is passed through literally.
fn shell_quote(value: &str) -> String {
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
        //
        // Gated on the function rather than with the `if !cfg!(unix) { return; }` its neighbours
        // use, because there is no non-`unix` half of it at all: `mode` is a documented no-op on
        // Windows (see `write_private_file`), so there is nothing there for this to assert.
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
        #[cfg(windows)]
        {
            // There is no mode to assert on Windows - access comes from the DACL the launch
            // directory inherits from the (per-user) temp directory, which `new_dir_owner_only`'s
            // Windows twin documents in full. What *is* assertable, and is the half of the
            // property this platform can actually get wrong, is that Jerry created a genuine
            // directory of its own rather than adopting whatever was already at the path.
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
        // a *live* instance's directory would remove the forwarder script out from under its
        // running agents.
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
        // The real reason the quoting exists - an unquoted path here would make Claude Code run
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
        if cfg!(windows) {
            // The quoted path is an argument to `powershell.exe`, so the command begins with the
            // interpreter rather than with the quote - see `windows_hook_entry`.
            assert!(command.starts_with("powershell.exe "), "got {command:?}");
            assert!(
                command.contains(&format!("-File '{}", spaced.display())),
                "the spaced directory must be quoted as one argument, got {command:?}"
            );
        } else {
            assert!(command.starts_with('\''), "got {command:?}");
        }

        if cfg!(unix) {
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
    // never fire", which is the bug this whole path exists to fix - covered by no suite anybody
    // routinely runs. Same call `crate::settings::state::windows_shell_suggestions` already makes.
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
        // symmetrically - so a literal opened with an ASCII `'` is closed just as happily by a `’`.
        //
        // The old quoter doubled only the ASCII one and was proved exploitable by execution: the
        // `hostile` path below, quoted by it, parses as three statements under a real Windows
        // PowerShell 5.1, the middle one live code running as the user on every tool call.
        //
        // Deliberately asserted as `is_err`, not as some cleverer escaping. Doubling the
        // typographic quotes as well "works" on 5.1, but the pairing is a lookahead that keeps the
        // *second* character (`’'` collapses to an ASCII `'`), is documented nowhere, and would
        // fail as a silently *different path* rather than as an error - see `powershell_quote`.
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
        // hardening exercise. `O’Brien` with a typographic apostrophe is a legal Windows profile
        // directory - the character is not reserved, and word processors, browsers and chat
        // clients autocorrect a straight quote into one. Under the old quoter the generated
        // command was a *parse error*, so a real PowerShell error reached the user's stderr on
        // every single hook firing, which is worse than the silent Phase 1 fallback it replaced.
        let obrien = "C:\\Users\\O\u{2019}Brien\\AppData\\Local\\Temp\\jerry-hook-forwarder.ps1";
        assert!(
            powershell_quote(obrien).is_err(),
            "a merely unlucky path must be refused too, so hooks are skipped rather than broken"
        );

        // Positive controls: everything that is *not* one of the five must still be quoted, and an
        // ordinary ASCII apostrophe is still handled by doubling rather than by refusing.
        assert_eq!(
            quoted(r"C:\Users\O'Brien\AppData\Local\Temp\jerry-hook-forwarder.ps1"),
            r"'C:\Users\O''Brien\AppData\Local\Temp\jerry-hook-forwarder.ps1'",
            "the straight apostrophe is escapable, and must not be caught by the refusal"
        );
        // The characters PowerShell's tokenizer special-cases *next door* to the quote class -
        // the three typographic double quotes, the dashes it accepts for a parameter `-`, and the
        // whitespace it accepts besides space and tab - are inert inside a literal, so refusing
        // them would only lose users hooks for nothing.
        for inert in [
            '\u{201c}', '\u{201d}', '\u{201e}', '\u{2013}', '\u{2014}', '\u{2015}', '\u{00a0}',
            '\u{0085}', '"', '`', '$',
        ] {
            let path = format!("C:\\Temp\\x{inert}y\\jerry-hook-forwarder.ps1");
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
        // into the same graceful Phase 1 fallback an unbindable port or an unwritable temp
        // directory takes. A refusal that got swallowed anywhere along the way would write a
        // settings file naming a command PowerShell cannot parse.
        //
        // Driven through `settings_json_with(.., windows_hook_entry)` so it runs on Linux too -
        // the same reason `windows_hook_entry` itself is compiled everywhere. On Windows,
        // `windows_only::hook_injection_is_skipped_when_the_launch_path_cannot_be_quoted` asserts
        // the identical property through the real `HookFiles::write_in`.
        let refused = Path::new("/tmp/O\u{2019}Brien/jerry-hooks-1-ab/jerry-hook-forwarder.ps1");
        let error = settings_json_with(refused, windows_hook_entry)
            .expect_err("an unquotable forwarder path must fail the whole settings file");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            error.to_string().contains("U+2019"),
            "the message reaching HookRuntime::start's log line must say what went wrong: {error}"
        );

        // And the ordinary path must still build a complete file, so the refusal cannot be a
        // blanket "Windows settings never generate".
        let fine = Path::new(r"C:\Temp\jerry-hooks-1-ab\jerry-hook-forwarder.ps1");
        let json = settings_json_with(fine, windows_hook_entry).expect("must still be generated");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(
            parsed["hooks"].as_object().map(serde_json::Map::len),
            Some(FORWARDED_EVENTS.len())
        );
    }

    #[test]
    fn a_hostile_path_cannot_break_out_of_the_generated_windows_command() {
        // The adversarial case. If a temp path could end the quoted region early, everything after
        // it becomes live PowerShell running as the user, fired by Claude Code on every tool call.
        // The property that makes that impossible for an ASCII apostrophe is that a single-quoted
        // PowerShell string has no internal escape sequence at all: only an *odd* run of quotes
        // can end it, and doubling guarantees every run is even. (The typographic quotes, which
        // *can* end it and which doubling does not save, are refused outright instead - see
        // `a_path_containing_any_of_powershells_four_other_quote_characters_is_refused_outright`.)
        let hostile = r"C:\Temp\x'; Start-Process calc.exe; '";
        let entry = windows_hook_entry(hostile, "Stop").expect("an ASCII apostrophe is quotable");
        let command = entry["command"].as_str().expect("a command string");

        let quoted = quoted(hostile);
        assert!(
            command.contains(&quoted),
            "the whole path must appear as one quoted literal, got {command:?}"
        );
        // Everything between the first and the last quote is the path; nothing hostile may sit
        // outside it.
        let first = command.find('\'').expect("an opening quote");
        let last = command.rfind('\'').expect("a closing quote");
        let (prefix, suffix) = (&command[..first], &command[last + 1..]);
        assert_eq!(
            prefix, "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File ",
            "nothing but the fixed interpreter invocation may precede the quoted path"
        );
        assert_eq!(
            suffix, " Stop",
            "nothing but the event name may follow the quoted path, got {suffix:?}"
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
            let entry =
                windows_hook_entry(r"C:\Temp\jerry-hooks-1-ab\jerry-hook-forwarder.ps1", event)
                    .expect("an ordinary Windows path must be quotable");
            assert_eq!(entry["type"], "command");
            assert_eq!(
                entry["shell"], "powershell",
                "{event}: the shell must be pinned, not inferred"
            );
            let command = entry["command"].as_str().expect("a command");
            assert!(
                command.ends_with(&format!(" {event}")),
                "{event}: the event name must reach the forwarder, got {command:?}"
            );
            assert!(
                command.contains("-ExecutionPolicy Bypass -File"),
                "{event}: a .ps1 is unrunnable under the default Restricted policy without this, \
                 and hooks would silently never fire - got {command:?}"
            );
            assert!(
                command.contains("-NoProfile"),
                "{event}: the user's PowerShell profile must not run on every hook"
            );
        }
    }

    #[test]
    // Flaky under the full parallel suite: GitHub issue #320 (ETXTBSY - another test's
    // Command::spawn can fork while this test's stand-in script is still open for writing).
    // Ignored here so `cargo test --workspace` in CI is a trustworthy gate; run directly with
    // `cargo test -p app the_generated_windows_command_tokenizes -- --ignored` to exercise it.
    #[ignore]
    fn the_generated_windows_command_tokenizes_identically_under_a_real_posix_shell() {
        // The module docs claim the Windows command string is deliberately *also* a valid Git Bash
        // command line, as insurance against a Claude Code that ignores the `shell` field. That is
        // a claim about a shell, so it is checked against a real one rather than asserted in prose
        // - and a POSIX `sh` is exactly the tokenizer Git Bash uses for the constructs involved
        // here (a bare command word plus single-quoted arguments).
        //
        // A stand-in `powershell.exe` on `PATH` prints its argument vector one element per line,
        // so this pins the real thing that matters: that a temp path full of spaces, parentheses,
        // dollars and backticks arrives as exactly *one* argument, unexpanded.
        if !cfg!(unix) {
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        write_private_file(
            &bin.join("powershell.exe"),
            b"#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done\n",
            0o700,
        )
        .expect("stand-in interpreter");

        let forwarder = r"C:\Program Files (x86)\a b\$HOME\`whoami`\jerry-hook-forwarder.ps1";
        let entry = windows_hook_entry(forwarder, "PreToolUse").expect("quotable");
        let command = entry["command"].as_str().expect("a command");

        // `write_private_file` above already closes its write handle (via `File`'s `Drop`,
        // driven off `sync_all`'s return) well before this point, so the standard "close the
        // write fd before exec'ing" fix is already in place for this test's own fd. The
        // remaining hazard (see issue #320) is `ETXTBSY` from a *different* test's
        // `Command::spawn` racing a `fork()` while this fd was briefly open: `fork()` duplicates
        // every fd in the whole process, not just the calling thread's, so under the full
        // parallel suite some other test's spawn can fork with our still-open write fd on
        // `powershell.exe` inherited into its child, and that child holds it open for writing
        // (Rust's `O_CLOEXEC` only closes it *at* that child's own `exec`, not at `fork`) for
        // the brief span until it execs its own target. If this test's own exec of the stand-in
        // lands in that span, `/bin/sh` reports `ETXTBSY` as "Text file busy" on its stderr and
        // exits non-zero. That span is a handful of microseconds and is outside this test's
        // control (it belongs to an unrelated test's spawn, not this one's), so retrying the
        // exec specifically on that signature - rather than looping on some fd of our own - is
        // the correct fix, not a band-aid: it is bounded, narrowly targeted at the one known
        // transient cause, and does not mask any other failure mode.
        let mut attempt = 0;
        let output = loop {
            attempt += 1;
            let output = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(command)
                .env("PATH", &bin)
                .stdin(std::process::Stdio::null())
                .output()
                .expect("the generated command must be runnable by a real POSIX shell");
            let is_etxtbsy = !output.status.success()
                && String::from_utf8_lossy(&output.stderr).contains("Text file busy");
            if !is_etxtbsy || attempt >= 20 {
                break output;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(
            output.status.success(),
            "the generated command must parse: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let argv: Vec<&str> = std::str::from_utf8(&output.stdout)
            .expect("utf-8")
            .lines()
            .collect();
        assert_eq!(
            argv,
            vec![
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                forwarder,
                "PreToolUse",
            ],
            "the forwarder path must arrive as exactly one unexpanded argument"
        );
    }

    #[test]
    fn the_unix_hook_entry_is_untouched_by_the_windows_one_existing() {
        // Both generators are compiled everywhere, so this is a real cross-platform guard against
        // the Windows work having quietly altered the shape Unix has shipped since issue #239.
        let entry = unix_hook_entry("/tmp/jerry-hooks-1-ab/jerry-hook-forwarder.sh", "Stop");
        assert_eq!(entry["type"], "command");
        assert!(
            entry.get("shell").is_none(),
            "Unix must keep declaring no shell - `sh -c` is already Claude Code's default there"
        );
        assert_eq!(
            entry["command"],
            "'/tmp/jerry-hooks-1-ab/jerry-hook-forwarder.sh' Stop"
        );
    }

    #[test]
    fn the_windows_forwarder_honours_the_same_contract_as_the_posix_one() {
        // The two scripts are different languages saying the same three things (see the module
        // docs). Asserted on the real script text so a future edit to either cannot quietly drop
        // one of them - on Linux, where the script can be read but not run.
        let script = WINDOWS_FORWARDER_SCRIPT;

        for variable in [PORT_ENV, TOKEN_ENV, AGENT_ENV] {
            assert!(
                script.contains(&format!("$env:{variable}")),
                "the forwarder must guard on and read ${variable} from the environment"
            );
            assert!(
                script.contains(&format!("if (-not $env:{variable}) {{ exit 0 }}")),
                "${variable} missing must make the forwarder a no-op, not a partial run"
            );
        }
        assert!(
            script.contains("if (-not $JerryHookEvent) { exit 0 }"),
            "a missing event name must no-op too, matching the `sh` script's `[ -n \"$1\" ]`"
        );
        assert!(
            script.trim_end().ends_with("exit 0"),
            "the last statement must be an unconditional `exit 0`"
        );
        assert!(
            !script.contains("ConvertFrom-Json"),
            "the forwarder must never parse the payload - that is what Rust's parser is for"
        );
        assert!(
            !script.contains("Invoke-Expression") && !script.contains("iex "),
            "nothing in the forwarder may evaluate a string as code"
        );
        assert!(
            script.contains("'--data-binary', '@-'"),
            "the payload must be posted verbatim from stdin, exactly as the `sh` forwarder does"
        );
        assert!(
            script.contains("$JerryStart.RedirectStandardOutput = $true")
                && script.contains("$JerryStart.RedirectStandardError = $true")
                && script
                    .contains("StandardOutput.BaseStream.CopyToAsync([System.IO.Stream]::Null)")
                && script
                    .contains("StandardError.BaseStream.CopyToAsync([System.IO.Stream]::Null)"),
            "curl's output must never reach Claude Code - stdout is fed back to the model as \
             context on some events, and stderr is shown to the user as a hook error - and both \
             must be drained rather than merely redirected, or a full pipe deadlocks the POST"
        );
    }

    #[test]
    fn the_windows_forwarder_never_writes_the_payload_to_disk_at_all() {
        // This replaces a weaker property. The forwarder used to spool stdin to
        // `payload-<guid>.json` in the launch directory and delete it in a `finally`, and the test
        // here asserted only that no such file was *left behind* after a normal run. A `finally`
        // does not run when the process is killed - a hook timeout, a pane closed mid-tool-call,
        // Jerry quit while a hook is in flight - and the payload is not innocuous: `PreToolUse`
        // carries `Write`/`Edit` file contents and whole `Bash` command lines, secrets included.
        //
        // "Never created" is both strictly stronger than "always cleaned up" and, unlike it,
        // checkable on Linux from the script text - which is where this suite actually runs.
        let script = WINDOWS_FORWARDER_SCRIPT;
        assert!(
            !script.contains("payload-"),
            "no spool file may be named anywhere in the forwarder"
        );
        for writer in [
            "[System.IO.File]::Open",
            "[System.IO.File]::Create",
            "[System.IO.File]::Write",
            "New-Item",
            "Set-Content",
            "Add-Content",
            "Out-File",
            "Remove-Item",
            "$PSScriptRoot",
        ] {
            assert!(
                !script.contains(writer),
                "the forwarder must never touch the filesystem, found {writer:?} - the payload \
                 goes straight into curl's stdin, so there is nothing to write or to clean up"
            );
        }
        assert!(
            script.contains("$JerryStart.RedirectStandardInput = $true")
                && script.contains("$JerryCurlProcess.StandardInput.BaseStream"),
            "the payload must be streamed into curl's stdin as raw bytes - `.BaseStream` is the \
             pipe under the StreamWriter, so no encoder ever sees it (see the module docs for why \
             every text-shaped route on Windows mangles a non-ASCII payload)"
        );
        assert!(
            !script.contains("param([string] $Event"),
            "the event parameter must not shadow PowerShell's `$Event` automatic variable"
        );
    }

    #[test]
    fn the_windows_forwarder_is_pure_ascii_so_a_bom_less_file_cannot_be_misread() {
        // Windows PowerShell 5.1 - still the `powershell.exe` every Windows ships - decodes a
        // `.ps1` with no byte-order mark using the system *ANSI* code page, not UTF-8.
        // `write_private_file` writes raw UTF-8 bytes and no BOM, so the moment this script grows
        // a non-ASCII character (a curly quote pasted into a comment is the realistic way it
        // happens) it is decoded as mojibake on a non-Western-European code page, and a mangled
        // string literal or comment can be a parse error - which means every hook silently stops
        // firing. ASCII is the one encoding every code page agrees with, so the fix is to stay
        // inside it rather than to start emitting a BOM.
        assert!(
            WINDOWS_FORWARDER_SCRIPT.is_ascii(),
            "the PowerShell forwarder must contain no non-ASCII byte"
        );
    }

    #[test]
    fn neither_forwarder_ever_embeds_the_token() {
        // The Unix version of this (`the_auth_token_is_never_written_into_any_generated_file`)
        // can only see the script this platform writes. This one holds both to the rule at once,
        // so a Windows-only regression cannot hide on a Linux CI run.
        for script in [UNIX_FORWARDER_SCRIPT, WINDOWS_FORWARDER_SCRIPT] {
            assert!(
                script.contains(TOKEN_ENV),
                "the token must be read from ${TOKEN_ENV} at run time"
            );
            let bearer = script
                .split("Bearer ")
                .nth(1)
                .expect("both forwarders send a bearer header");
            assert!(
                bearer.starts_with(&format!("${TOKEN_ENV}"))
                    || bearer.starts_with(&format!("$($env:{TOKEN_ENV})")),
                "the bearer header must interpolate the environment variable, never a literal: \
                 {bearer:.40?}"
            );
        }
    }

    #[test]
    fn the_two_forwarders_have_distinct_names_so_neither_can_be_run_by_the_wrong_interpreter() {
        assert_ne!(UNIX_FORWARDER_NAME, WINDOWS_FORWARDER_NAME);
        assert!(WINDOWS_FORWARDER_NAME.ends_with(".ps1"));
        assert!(UNIX_FORWARDER_NAME.ends_with(".sh"));
        assert_eq!(
            FORWARDER_NAME,
            if cfg!(windows) {
                WINDOWS_FORWARDER_NAME
            } else {
                UNIX_FORWARDER_NAME
            },
            "this platform must write the script it can actually run"
        );
    }

    #[test]
    fn hook_injection_is_supported_on_every_platform_that_has_a_forwarder() {
        assert_eq!(is_supported(), cfg!(unix) || cfg!(windows));
        if cfg!(windows) {
            assert!(
                is_supported(),
                "native Windows really does install hooks now - a `false` here is the exact \
                 regression that left Windows users on the Phase 1 heuristics"
            );
        }
    }

    /// The tests that genuinely need a Windows kernel: a real `powershell.exe` to run the real
    /// generated script, and real Win32 process handles.
    #[cfg(windows)]
    mod windows_only {
        use super::*;

        /// The real generated `Stop` command, read back out of the real generated settings file
        /// rather than reconstructed - so this exercises the string Claude Code would actually
        /// run - wrapped in the shell `"shell": "powershell"` asks Claude Code for.
        pub(super) fn generated_stop_command(files: &HookFiles) -> std::process::Command {
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
        fn the_generated_forwarder_is_a_powershell_script_that_really_exists() {
            let temp = tempfile::tempdir().expect("temp dir");
            let files = HookFiles::write_in(temp.path()).expect("must write");
            let forwarder = files.settings_path().with_file_name(WINDOWS_FORWARDER_NAME);
            assert!(
                forwarder.is_file(),
                "the .ps1 the settings file names must really be on disk"
            );
            assert_eq!(
                std::fs::read_to_string(&forwarder).expect("read"),
                WINDOWS_FORWARDER_SCRIPT
            );
        }

        #[test]
        fn the_forwarder_no_ops_without_jerry_env_vars_and_never_reports_failure() {
            // The Windows twin of the identically-named test above: the property that makes the
            // generated command safe to run outside Jerry.
            let temp = tempfile::tempdir().expect("temp dir");
            let files = HookFiles::write_in(temp.path()).expect("must write");
            let output = generated_stop_command(&files)
                .env_remove(PORT_ENV)
                .env_remove(TOKEN_ENV)
                .env_remove(AGENT_ENV)
                .stdin(std::process::Stdio::null())
                .output()
                .expect("the generated command must run");
            assert!(
                output.status.success(),
                "the forwarder must exit 0 outside Jerry, got {:?}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stdout.is_empty(),
                "it must produce no output that Claude Code would feed back as context, got {:?}",
                String::from_utf8_lossy(&output.stdout)
            );
        }

        #[test]
        fn the_forwarder_exits_zero_even_when_the_listener_is_dead() {
            // A hook that exits non-zero blocks the agent's tool call. Jerry having quit must
            // never do that.
            let temp = tempfile::tempdir().expect("temp dir");
            let files = HookFiles::write_in(temp.path()).expect("must write");
            let dead_port = {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
                listener.local_addr().expect("addr").port()
            };

            let mut child = generated_stop_command(&files)
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
        fn the_forwarder_never_puts_the_payload_on_disk_even_while_it_is_in_flight() {
            // The forwarder used to spool stdin to a file in the launch directory and delete it in
            // a `finally`, and this test used to check only that nothing was left over afterwards.
            // A `finally` does not run for a killed process, and a `PreToolUse` payload holds real
            // file contents and whole `Bash` command lines - so the property was strengthened from
            // "cleaned up" to "never written", which this asserts *while the POST is still in
            // flight* rather than only after it: the run below points at a real listener that
            // accepts the connection and then never answers, so curl is still holding the body
            // when the directory is inspected.
            let temp = tempfile::tempdir().expect("temp dir");
            let files = HookFiles::write_in(temp.path()).expect("must write");
            let directory = files
                .settings_path()
                .parent()
                .expect("parent")
                .to_path_buf();

            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            let port = listener.local_addr().expect("addr").port();

            let mut child = generated_stop_command(&files)
                .env(PORT_ENV, port.to_string())
                .env(TOKEN_ENV, "irrelevant")
                .env(AGENT_ENV, "1")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn");
            {
                use std::io::Write;
                let mut stdin = child.stdin.take().expect("stdin");
                stdin
                    .write_all(br#"{"hook_event_name":"PreToolUse","tool_input":{"command":"export AWS_SECRET_ACCESS_KEY=hunter2"}}"#)
                    .ok();
            }

            let mut seen: Vec<String> = Vec::new();
            for _ in 0..20 {
                seen.extend(
                    std::fs::read_dir(&directory)
                        .expect("read launch dir")
                        .flatten()
                        .map(|entry| entry.file_name().to_string_lossy().into_owned())
                        .filter(|name| name != SETTINGS_NAME && name != WINDOWS_FORWARDER_NAME),
                );
                if !seen.is_empty() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let output = child.wait_with_output().expect("wait");
            seen.extend(
                std::fs::read_dir(&directory)
                    .expect("read launch dir")
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name != SETTINGS_NAME && name != WINDOWS_FORWARDER_NAME),
            );
            assert!(
                seen.is_empty(),
                "the payload must never reach the filesystem, not even transiently: {seen:?}"
            );
            assert!(
                output.status.success(),
                "and the hook must still exit 0: {:?} {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.stdout.is_empty() && output.stderr.is_empty(),
                "a listener that accepts and never answers must still print nothing at the user"
            );
            drop(listener);
        }

        #[test]
        fn hook_injection_is_skipped_when_the_launch_path_cannot_be_quoted() {
            // The Windows end of `a_refusal_to_quote_skips_hook_injection_entirely_rather_than_
            // writing_a_broken_file`, through the real `HookFiles::write_in` rather than the test
            // seam: a temp directory whose name contains a typographic apostrophe (legal on
            // Windows, and the realistic `C:\Users\O’Brien` case) must make the whole thing fail,
            // so `HookRuntime::start` logs it and falls back, instead of a settings file being
            // written that names a command PowerShell cannot parse.
            let temp = tempfile::tempdir().expect("temp dir");
            let parent = temp.path().join("O\u{2019}Brien");
            std::fs::create_dir_all(&parent).expect("create");
            let error = HookFiles::write_in(&parent)
                .expect_err("an unquotable launch path must not produce a settings file");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

            // And nothing may be left behind: no half-written settings file, and not even the
            // launch directory, which nothing would ever collect (its pid is this live process's,
            // and `sweep_stale_directories` deliberately leaves a live pid's directories alone).
            let leftovers: Vec<_> = std::fs::read_dir(&parent)
                .expect("read parent")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                leftovers.is_empty(),
                "a refusal must take its half-built launch directory back down: {leftovers:?}"
            );
        }

        #[test]
        fn a_live_windows_process_reads_as_alive_and_a_freed_pid_does_not() {
            // `process_is_alive` decides whether `sweep_stale_directories` deletes a directory, so
            // a wrong "dead" removes a *running* Jerry's forwarder script out from under its
            // agents. Exercised against a real child process rather than a constant.
            // `Start-Sleep`, not `timeout.exe`: `timeout` refuses to run at all when stdin is
            // redirected ("ERROR: Input redirection is not supported"), which would make the child
            // exit instantly and this test flakily assert liveness against an already-dead pid.
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
            // The pid is only definitively free once the last handle is closed; `Child`'s own
            // handle is dropped here.
            drop(child);

            // A pid that cannot plausibly exist. Windows pids are multiples of 4 and well below
            // this, so `OpenProcess` reports `ERROR_INVALID_PARAMETER` for it.
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
            // The Windows half of `a_pre_planted_symlink_cannot_capture_the_generated_files`:
            // `CreateDirectoryW` must refuse an existing path rather than adopt it.
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
