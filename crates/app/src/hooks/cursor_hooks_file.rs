//! Read-modify-write merge of `~/.cursor/hooks.json` (GitHub issue #479) - the Cursor Agent CLI's
//! own, single global hooks file. Unlike `crate::hooks::settings_file`'s `--settings <path>` file
//! (entirely Jerry-owned, regenerated whole on every launch), `hooks.json` is shared with the user
//! and possibly other tools, so this is a real surgical merge: read it, touch only Jerry's own
//! entries, write it back - or abort untouched if it can't be parsed at all.
//!
//! Own-entries are identified the same way Orca and Superset (see issue #479's design comment)
//! identify theirs: the managed forwarder script's own path appearing as a substring of an
//! entry's `command` string - `hooks.json` entries are bare `{command, timeout}` objects with no
//! room for a marker field. The substring matched is deliberately the forwarder's *stable parent
//! directory* ([`managed_marker`]), not its exact version-stamped file name - the same reasoning
//! `crate::hooks::settings_file`'s `DIRECTORY_PREFIX`/`sweep_stale_directories` already uses for
//! the Claude forwarder's per-launch directories: an entry written by an older Jerry version still
//! lives under the same stable directory, so a later [`install`] sweeps and replaces it instead of
//! it accumulating as an orphan every time the forwarder's own file name is stamped forward with a
//! new version.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::hooks::settings_file::{powershell_quote, random_suffix, shell_quote};
use crate::settings::store::home_dir;

/// The hook events Jerry subscribes to in `~/.cursor/hooks.json` - deliberately not the CLI's
/// full ~18-event set. See GitHub issue #479's design comment:
/// - Not `sessionStart`/`sessionEnd`: Orca's own hazard note (a process-boundary session hook can
///   reset turn-tracking state across a resumed session) applies just as much to Jerry, whose
///   Cursor sessions are also minted via `--resume`/`create-chat`
///   ([`crate::work_surface::agents::AgentKind::mints_chat_id`]).
/// - Not `beforeShellExecution`/`beforeMCPExecution`: both fire *before* Cursor's own permission
///   decision, so they cannot distinguish "about to block on a human" from "about to auto-run
///   under trust" - subscribing would make [`crate::hooks::event::HookFact::NeedsInput`] a
///   false-positive machine rather than a real signal. `NeedsInput` stays on the existing
///   terminal-title/quiescence fallback (`crate::rail::status`) - no change needed there.
pub const CURSOR_HOOK_EVENTS: [&str; 6] = [
    "beforeSubmitPrompt",
    "stop",
    "preToolUse",
    "postToolUse",
    "postToolUseFailure",
    "afterAgentResponse",
];

/// Timeout Jerry declares on each of its own hook entries, in seconds. The forwarder itself never
/// blocks long (a real `--max-time 5` curl call, mirroring
/// `crate::hooks::settings_file`'s forwarder scripts) - this is a generous ceiling so a slow or
/// loaded machine's hook call is never itself the thing that trips Cursor's own timeout handling.
const HOOK_TIMEOUT_SECS: u64 = 30;

/// The stable directory name (under Jerry's own config dir) the Cursor forwarder script lives in.
/// Deliberately never version-stamped itself - only the file inside it is (see
/// [`forwarder_path`]) - because this directory's own path is what [`managed_marker`] matches
/// entries against, and it must stay the same across every Jerry version for the sweep-old-entries
/// property described in this module's own docs to hold.
const FORWARDER_DIR_NAME: &str = "cursor-hooks";

/// `~/.cursor/hooks.json` - the one location `cursor-agent` actually reads a global hooks file
/// from. There is no CLI flag or environment variable that points it elsewhere (GitHub issue
/// #479's research comment).
pub fn hooks_json_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".cursor").join("hooks.json"))
}

/// This build's Cursor forwarder script path: under Jerry's own config dir
/// (`~/.config/jerry/cursor-hooks/`, mirroring `crate::settings::store::settings_toml_path`'s own
/// `~/.config/jerry/...` convention), version-stamped by file name so an upgrade's freshly written
/// script never collides with - or is confused for - an older one still referenced by a stale
/// `hooks.json` entry. Never deleted on drop: unlike
/// `crate::hooks::settings_file::HookFiles`'s per-launch directory, `~/.cursor/hooks.json`
/// outlives any single Jerry process and has to keep pointing at a real file between launches.
pub fn forwarder_path() -> Option<PathBuf> {
    let extension = if cfg!(windows) { "ps1" } else { "sh" };
    Some(
        home_dir()?
            .join(".config")
            .join("jerry")
            .join(FORWARDER_DIR_NAME)
            .join(format!(
                "jerry-cursor-hook-forwarder-{}.{extension}",
                env!("CARGO_PKG_VERSION")
            )),
    )
}

/// Writes this build's forwarder script to `path`, creating its parent directory if needed - a
/// no-op if `path` already holds exactly this content, so repeated calls (once per launch, per
/// [`crate::hooks`]'s module docs) don't needlessly touch the file's mtime.
pub fn ensure_forwarder_written(path: &Path) -> io::Result<()> {
    let contents = if cfg!(windows) {
        CURSOR_WINDOWS_FORWARDER_SCRIPT
    } else {
        CURSOR_UNIX_FORWARDER_SCRIPT
    };
    if let Ok(existing) = std::fs::read(path) {
        if existing == contents.as_bytes() {
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(path, contents.as_bytes())?;
    // Owner-executable, matching `crate::hooks::settings_file::write_private_file`'s own reasoning
    // for the Claude forwarder: some shells Cursor might invoke this through resolve it as a
    // program rather than always going through an explicit interpreter.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Merges Jerry's 6 managed entries into `cursor_hooks_json_path`, pointing every one at
/// `forwarder_path` - see this module's own docs for the merge/identity/sweep rules. Idempotent:
/// running this twice in a row with the same `forwarder_path` writes nothing the second time.
///
/// Never clobbers a file it cannot parse: if `cursor_hooks_json_path` exists but is not valid
/// JSON, or its root is not a JSON object, this returns `Ok(())` having touched nothing at all -
/// exactly [`crate::hooks::event::parse`]'s own "gracefully decline" shape, not an error, because
/// "the user's hand-edited file is currently broken" isn't a failure of *this* operation.
pub fn install(cursor_hooks_json_path: &Path, forwarder_path: &Path) -> io::Result<()> {
    let Some((mut root, existing_raw)) = load_mergeable(cursor_hooks_json_path, true)? else {
        return Ok(());
    };
    let Some(object) = root.as_object_mut() else {
        return Ok(());
    };
    object.entry("version").or_insert(Value::from(1));
    let hooks_value = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks_obj) = hooks_value.as_object_mut() else {
        return Ok(());
    };

    let marker = managed_marker(forwarder_path);
    for event in CURSOR_HOOK_EVENTS {
        let command = managed_command(forwarder_path, event)?;
        let entries = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(array) = entries.as_array_mut() else {
            return Ok(());
        };
        array.retain(|entry| !is_managed_entry(entry, &marker));
        array.push(serde_json::json!({ "command": command, "timeout": HOOK_TIMEOUT_SECS }));
    }

    write_if_changed(cursor_hooks_json_path, existing_raw.as_deref(), &root)
}

/// Strips every entry [`install`] would have written, under every one of [`CURSOR_HOOK_EVENTS`],
/// identified the same way [`install`] identifies them - dropping an event's own key entirely if
/// removing Jerry's entry leaves it empty, but never touching a key that still holds real
/// user-authored entries. A complete no-op - it doesn't even open the file - when
/// `cursor_hooks_json_path` doesn't exist, and the same "abort untouched" behaviour as [`install`]
/// when it exists but isn't parseable.
pub fn remove_managed_entries(
    cursor_hooks_json_path: &Path,
    forwarder_path: &Path,
) -> io::Result<()> {
    let Some((mut root, existing_raw)) = load_mergeable(cursor_hooks_json_path, false)? else {
        return Ok(());
    };
    let Some(object) = root.as_object_mut() else {
        return Ok(());
    };
    let Some(hooks_obj) = object.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };

    let marker = managed_marker(forwarder_path);
    let mut now_empty = Vec::new();
    for event in CURSOR_HOOK_EVENTS {
        let Some(array) = hooks_obj.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        array.retain(|entry| !is_managed_entry(entry, &marker));
        if array.is_empty() {
            now_empty.push(event);
        }
    }
    for event in now_empty {
        hooks_obj.remove(event);
    }

    write_if_changed(cursor_hooks_json_path, existing_raw.as_deref(), &root)
}

/// Reads and parses `path` into `(root value, original raw text)`. `Ok(None)` means "the caller
/// must abort without writing" - either the content isn't a parseable JSON object, or the file
/// doesn't exist and `create_if_missing` is `false` ([`remove_managed_entries`]'s "don't create
/// what isn't there" contract). `create_if_missing` is `true` from [`install`], which synthesizes
/// a fresh empty object instead.
fn load_mergeable(
    path: &Path,
    create_if_missing: bool,
) -> io::Result<Option<(Value, Option<String>)>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Ok(value) if value.is_object() => Ok(Some((value, Some(raw)))),
            _ => {
                log::warn!(
                    "{} is not a valid JSON object, so Jerry will not touch it - fix or remove it \
                     by hand, or Cursor agent status will keep using the terminal/quiescence \
                     fallback until it's readable again",
                    path.display()
                );
                Ok(None)
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if create_if_missing {
                Ok(Some((Value::Object(Map::new()), None)))
            } else {
                Ok(None)
            }
        }
        Err(err) => Err(err),
    }
}

/// Serializes `root` and writes it to `path` only if that differs from `existing_raw` - the
/// no-op-on-no-change half of both [`install`] and [`remove_managed_entries`]'s idempotency.
fn write_if_changed(path: &Path, existing_raw: Option<&str>, root: &Value) -> io::Result<()> {
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(root)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?
    );
    if existing_raw == Some(rendered.as_str()) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_atomic(path, rendered.as_bytes())
}

/// Writes `contents` to a temp file beside `path`, then renames it into place - so a reader (the
/// user's editor, `cursor-agent` itself starting mid-write) never observes a half-written file.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let temp_name = match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => format!(".{name}.jerry-tmp-{}", random_suffix()),
        None => format!(".jerry-tmp-{}", random_suffix()),
    };
    let temp_path = parent.join(temp_name);
    std::fs::write(&temp_path, contents)?;
    std::fs::rename(&temp_path, path)
}

/// The substring an entry's `command` must contain to be considered Jerry's own - the forwarder's
/// *parent directory*, not its exact (version-stamped) file name. See this module's own top-level
/// docs for why that's the deliberate choice.
fn managed_marker(forwarder_path: &Path) -> String {
    forwarder_path
        .parent()
        .unwrap_or(forwarder_path)
        .to_string_lossy()
        .into_owned()
}

/// Whether `entry`'s `command` field contains `marker` - [`managed_marker`]'s consumer.
fn is_managed_entry(entry: &Value, marker: &str) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.contains(marker))
}

/// The real `command` string for one event's managed entry - `<forwarder> <event>` on POSIX
/// (quoted via [`shell_quote`], reused verbatim from `crate::hooks::settings_file` rather than
/// re-implementing the same escaping), or the same `powershell.exe -File <forwarder> <event>`
/// shape `crate::hooks::settings_file::windows_hook_entry` already builds for Claude's settings
/// file - deliberately parallel, reusing [`powershell_quote`] rather than a second quoter, since
/// `hooks.json`'s `command` field is executed the same way a shell command line is.
fn managed_command(forwarder_path: &Path, event: &str) -> io::Result<String> {
    let forwarder = forwarder_path.to_string_lossy();
    if cfg!(windows) {
        let quoted = powershell_quote(&forwarder)?;
        Ok(format!(
            "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {quoted} {event}"
        ))
    } else {
        Ok(format!("{} {event}", shell_quote(&forwarder)))
    }
}

/// The POSIX forwarder - deliberately parallel to
/// `crate::hooks::settings_file::UNIX_FORWARDER_SCRIPT`: same safety contract (a no-op without the
/// `JERRY_*` environment, never propagates curl's exit status, always exits 0), only the request
/// path differs (`/hook/cursor`, not `/hook` - see `crate::hooks::server`'s second route).
pub const CURSOR_UNIX_FORWARDER_SCRIPT: &str = r#"#!/bin/sh
# Written by Jerry (github.com/ColinEspinas/jerry) to forward Cursor Agent CLI hook payloads to
# the Jerry instance that spawned this agent (GitHub issue #479). Safe to run anywhere, including
# from a `cursor-agent` session started outside Jerry - `~/.cursor/hooks.json` is one global file,
# so without the JERRY_* environment variables Jerry injects on the panes it spawns, this exits
# immediately having done nothing.
[ -n "$JERRY_HOOK_PORT" ] || exit 0
[ -n "$JERRY_HOOK_TOKEN" ] || exit 0
[ -n "$JERRY_AGENT_ID" ] || exit 0
[ -n "$1" ] || exit 0
command -v curl >/dev/null 2>&1 || exit 0

# Never propagate curl's exit status: a non-zero exit here would surface as a Cursor hook error. A
# dead listener must cost nothing. Cursor pipes the hook's JSON payload on this script's own
# stdin; `--data-binary @-` reads it straight through to curl, never buffered on disk.
curl --silent --show-error --output /dev/null --max-time 5 \
  --request POST \
  --header "Authorization: Bearer $JERRY_HOOK_TOKEN" \
  --header "Content-Type: application/json" \
  --data-binary @- \
  "http://127.0.0.1:$JERRY_HOOK_PORT/hook/cursor?event=$1&agent=$JERRY_AGENT_ID" >/dev/null 2>&1

exit 0
"#;

/// The Windows PowerShell forwarder - deliberately parallel to
/// `crate::hooks::settings_file::WINDOWS_FORWARDER_SCRIPT`: identical contract, identical
/// PowerShell-5.1-on-.NET-Framework argument-escaping approach (see that script's own comments for
/// the full reasoning, not repeated here), only the request path differs (`/hook/cursor`).
pub const CURSOR_WINDOWS_FORWARDER_SCRIPT: &str = r#"# Written by Jerry (github.com/ColinEspinas/jerry) to forward Cursor Agent CLI hook payloads to
# the Jerry instance that spawned this agent (GitHub issue #479). Safe to run anywhere, including
# from a `cursor-agent` session started outside Jerry - `~/.cursor/hooks.json` is one global file,
# so without the JERRY_* environment variables Jerry injects on the panes it spawns, this exits
# immediately having done nothing.
param([string] $JerryHookEvent = '')

$ErrorActionPreference = 'SilentlyContinue'
$ProgressPreference = 'SilentlyContinue'

if (-not $env:JERRY_HOOK_PORT) { exit 0 }
if (-not $env:JERRY_HOOK_TOKEN) { exit 0 }
if (-not $env:JERRY_AGENT_ID) { exit 0 }
if (-not $JerryHookEvent) { exit 0 }

# Same CreateProcess-argument-vector escaping as `crate::hooks::settings_file`'s Claude forwarder -
# see that script's own comments for why this is spelled out by hand rather than handed to
# ProcessStartInfo.ArgumentList, which does not exist under Windows PowerShell 5.1's .NET
# Framework runtime.
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
    $JerryCurl = Join-Path -Path "$env:SystemRoot" -ChildPath 'System32\curl.exe'
    if (-not (Test-Path -LiteralPath $JerryCurl -PathType Leaf)) {
        $JerryFound = @(Get-Command -Name 'curl.exe' -CommandType Application -ErrorAction SilentlyContinue)
        if ($JerryFound.Count -eq 0) { exit 0 }
        $JerryCurl = $JerryFound[0].Source
    }

    $JerryArgs = @(
        '--silent',
        '--max-time', '5',
        '--request', 'POST',
        '--header', "Authorization: Bearer $($env:JERRY_HOOK_TOKEN)",
        '--header', 'Content-Type: application/json',
        '--data-binary', '@-',
        "http://127.0.0.1:$($env:JERRY_HOOK_PORT)/hook/cursor?event=$JerryHookEvent&agent=$($env:JERRY_AGENT_ID)"
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

    [void] $JerryCurlProcess.StandardOutput.BaseStream.CopyToAsync([System.IO.Stream]::Null)
    [void] $JerryCurlProcess.StandardError.BaseStream.CopyToAsync([System.IO.Stream]::Null)

    $JerryStdin = [Console]::OpenStandardInput()
    try { $JerryStdin.CopyTo($JerryCurlProcess.StandardInput.BaseStream) }
    finally { $JerryCurlProcess.StandardInput.Close() }

    if (-not $JerryCurlProcess.WaitForExit(10000)) { $JerryCurlProcess.Kill() }
} catch {
    # Deliberately swallowed - see `crate::hooks::settings_file::WINDOWS_FORWARDER_SCRIPT`'s own
    # comment for why every path out of this file must exit 0.
} finally {
    if ($JerryCurlProcess) { $JerryCurlProcess.Dispose() }
}

exit 0
"#;

#[cfg(test)]
mod cursor_hooks_file_tests {
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    use super::{
        ensure_forwarder_written, install, managed_command, remove_managed_entries,
        CURSOR_HOOK_EVENTS, FORWARDER_DIR_NAME, HOOK_TIMEOUT_SECS,
    };

    /// A forwarder path under a fresh tempdir's own `cursor-hooks` directory - mirrors what
    /// [`forwarder_path`] would produce, without touching the real home directory.
    fn test_forwarder(root: &Path, version: &str) -> PathBuf {
        root.join(FORWARDER_DIR_NAME)
            .join(format!("jerry-cursor-hook-forwarder-{version}.sh"))
    }

    #[test]
    fn a_fresh_or_missing_file_gets_created_with_the_six_managed_entries_and_version_1() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");

        install(&hooks_json, &forwarder).expect("install must succeed");

        let raw = std::fs::read_to_string(&hooks_json).expect("file must exist");
        let parsed: Value = serde_json::from_str(&raw).expect("must be valid JSON");
        assert_eq!(parsed["version"], 1);
        let hooks = parsed["hooks"].as_object().expect("a hooks object");
        assert_eq!(hooks.len(), CURSOR_HOOK_EVENTS.len());
        for event in CURSOR_HOOK_EVENTS {
            let entries = hooks[event].as_array().expect("an array");
            assert_eq!(entries.len(), 1, "{event}: exactly one managed entry");
            let command = entries[0]["command"].as_str().expect("a command string");
            assert!(command.contains(&forwarder.to_string_lossy().into_owned()));
            assert!(command.ends_with(&format!(" {event}")));
            assert_eq!(entries[0]["timeout"], HOOK_TIMEOUT_SECS);
        }
    }

    #[test]
    fn unrelated_user_authored_hooks_survive_byte_for_byte_alongside_managed_entries() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");

        let original = serde_json::json!({
            "version": 2,
            "hooks": {
                // A different event key entirely, untouched by Jerry.
                "sessionStart": [{ "command": "/usr/local/bin/some-other-tool --notify", "timeout": 10 }],
                // The *same* event key Jerry manages, but a real user-authored command under it.
                "stop": [{ "command": "/home/user/.bin/on-stop.sh", "timeout": 5 }],
            }
        });
        std::fs::write(
            &hooks_json,
            serde_json::to_string_pretty(&original).expect("serialize"),
        )
        .expect("seed file");

        install(&hooks_json, &forwarder).expect("install must succeed");

        let raw = std::fs::read_to_string(&hooks_json).expect("read");
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        // The user's pinned version survives - only defaulted when absent.
        assert_eq!(parsed["version"], 2);
        let session_start = parsed["hooks"]["sessionStart"]
            .as_array()
            .expect("sessionStart array");
        assert_eq!(session_start.len(), 1);
        assert_eq!(
            session_start[0]["command"],
            "/usr/local/bin/some-other-tool --notify"
        );
        let stop = parsed["hooks"]["stop"].as_array().expect("stop array");
        assert_eq!(stop.len(), 2, "the user's own stop entry plus Jerry's own");
        assert!(stop
            .iter()
            .any(|entry| entry["command"] == "/home/user/.bin/on-stop.sh"));
        assert!(stop.iter().any(|entry| entry["command"]
            .as_str()
            .is_some_and(|command| command.contains(&forwarder.to_string_lossy().into_owned()))));
    }

    #[test]
    fn a_stale_entry_from_an_older_forwarder_version_under_the_same_stable_directory_is_swept() {
        // Decision (a) from GitHub issue #479's design comment: matching is on the forwarder's
        // stable *parent directory*, not its exact version-stamped file name - the same reasoning
        // `crate::hooks::settings_file`'s `DIRECTORY_PREFIX`/`sweep_stale_directories` already
        // uses for the Claude forwarder's launch directories. An entry a previous Jerry version
        // wrote must be swept and replaced, not accumulate as an orphan on every upgrade.
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let old_forwarder = test_forwarder(temp.path(), "0.1.0");
        let new_forwarder = test_forwarder(temp.path(), "0.2.0");

        install(&hooks_json, &old_forwarder).expect("first install");
        install(&hooks_json, &new_forwarder).expect("second install, newer version");

        let raw = std::fs::read_to_string(&hooks_json).expect("read");
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        for event in CURSOR_HOOK_EVENTS {
            let entries = parsed["hooks"][event].as_array().expect("an array");
            assert_eq!(
                entries.len(),
                1,
                "{event}: the old version's entry must be swept, not accumulated"
            );
            assert!(entries[0]["command"].as_str().is_some_and(
                |command| command.contains(&new_forwarder.to_string_lossy().into_owned())
            ));
        }
    }

    #[test]
    fn an_unrelated_tools_entry_under_a_different_directory_is_never_touched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");

        let original = serde_json::json!({
            "hooks": { "stop": [{ "command": "/opt/someone-elses/tool/hook.sh stop", "timeout": 5 }] }
        });
        std::fs::write(
            &hooks_json,
            serde_json::to_string_pretty(&original).expect("serialize"),
        )
        .expect("seed file");

        install(&hooks_json, &forwarder).expect("install");
        remove_managed_entries(&hooks_json, &forwarder).expect("remove");

        let raw = std::fs::read_to_string(&hooks_json).expect("read");
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        let stop = parsed["hooks"]["stop"].as_array().expect("stop array");
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["command"], "/opt/someone-elses/tool/hook.sh stop");
    }

    #[test]
    fn unparseable_existing_json_is_left_byte_for_byte_untouched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");
        std::fs::write(&hooks_json, b"{ not valid json at all").expect("seed file");
        let before = std::fs::read(&hooks_json).expect("read before");

        let result = install(&hooks_json, &forwarder);

        assert!(result.is_ok(), "must not surface a hard error: {result:?}");
        let after = std::fs::read(&hooks_json).expect("read after");
        assert_eq!(before, after, "an unparseable file must never be rewritten");
    }

    #[test]
    fn a_json_array_root_is_also_refused_rather_than_treated_as_mergeable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");
        std::fs::write(&hooks_json, b"[]").expect("seed file");

        install(&hooks_json, &forwarder).expect("must not error");

        let after = std::fs::read(&hooks_json).expect("read after");
        assert_eq!(after, b"[]", "a non-object root must never be rewritten");
    }

    #[test]
    fn installing_twice_in_a_row_is_a_real_no_op_the_second_time() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");

        install(&hooks_json, &forwarder).expect("first install");
        let mtime_after_first = std::fs::metadata(&hooks_json)
            .expect("metadata")
            .modified()
            .expect("mtime");

        // A real, observable delay so a second write (if one happened) would produce a strictly
        // later mtime on every platform's filesystem timestamp resolution.
        std::thread::sleep(std::time::Duration::from_millis(20));
        install(&hooks_json, &forwarder).expect("second install");
        let mtime_after_second = std::fs::metadata(&hooks_json)
            .expect("metadata")
            .modified()
            .expect("mtime");

        assert_eq!(
            mtime_after_first, mtime_after_second,
            "the second install must not have written the file at all"
        );
    }

    #[test]
    fn remove_managed_entries_drops_now_empty_event_keys_but_leaves_other_user_keys_alone() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");

        let original = serde_json::json!({
            "hooks": {
                "sessionStart": [{ "command": "/usr/local/bin/some-other-tool", "timeout": 10 }],
                "stop": [{ "command": "/home/user/.bin/on-stop.sh", "timeout": 5 }],
            }
        });
        std::fs::write(
            &hooks_json,
            serde_json::to_string_pretty(&original).expect("serialize"),
        )
        .expect("seed file");
        install(&hooks_json, &forwarder).expect("install");

        remove_managed_entries(&hooks_json, &forwarder).expect("remove");

        let raw = std::fs::read_to_string(&hooks_json).expect("read");
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        let hooks = parsed["hooks"].as_object().expect("hooks object");
        // Every event Jerry manages but that had no user entry alongside it must be gone
        // entirely, not left as an empty array.
        for event in CURSOR_HOOK_EVENTS {
            if event == "stop" {
                continue;
            }
            assert!(
                !hooks.contains_key(event),
                "{event}: must be removed entirely"
            );
        }
        // `stop` had a real user entry too, so the key survives with just that entry left.
        let stop = hooks["stop"].as_array().expect("stop array");
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["command"], "/home/user/.bin/on-stop.sh");
        // Untouched, unrelated key.
        assert_eq!(
            hooks["sessionStart"][0]["command"],
            "/usr/local/bin/some-other-tool"
        );
    }

    #[test]
    fn remove_managed_entries_on_a_missing_file_creates_nothing() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let forwarder = test_forwarder(temp.path(), "0.1.0");

        remove_managed_entries(&hooks_json, &forwarder).expect("must not error");

        assert!(!hooks_json.exists(), "nothing must be created");
    }

    #[test]
    fn ensure_forwarder_written_produces_a_real_owner_executable_file_and_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp dir");
        let path = temp.path().join(FORWARDER_DIR_NAME).join("forwarder.sh");

        ensure_forwarder_written(&path).expect("first write");
        assert!(path.is_file());
        let mtime_first = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");

        std::thread::sleep(std::time::Duration::from_millis(20));
        ensure_forwarder_written(&path).expect("second write");
        let mtime_second = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        assert_eq!(
            mtime_first, mtime_second,
            "identical content must not be rewritten"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = path.metadata().expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn the_managed_command_carries_the_event_name_and_the_forwarder_path() {
        let forwarder = PathBuf::from(
            "/home/user/.config/jerry/cursor-hooks/jerry-cursor-hook-forwarder-0.1.0.sh",
        );
        for event in CURSOR_HOOK_EVENTS {
            let command = managed_command(&forwarder, event).expect("must build");
            if cfg!(windows) {
                assert!(command.starts_with("powershell.exe "));
            } else {
                assert!(command.contains(&forwarder.to_string_lossy().into_owned()));
            }
            assert!(command.ends_with(&format!(" {event}")));
        }
    }
}
