//! Read-modify-write merge of `~/.cursor/hooks.json` (GitHub issue #479) - the Cursor Agent CLI's
//! own, single global hooks file. Unlike `crate::hooks::settings_file`'s `--settings <path>` file
//! (entirely Jerry-owned, regenerated whole on every launch), `hooks.json` is shared with the user
//! and possibly other tools, so this is a real surgical merge: read it, touch only Jerry's own
//! entries, write it back - or abort untouched if it can't be parsed at all.
//!
//! Since decision Q10 (`docs/architecture/decisions.md` §19) every managed entry runs
//! `<located jerry binary> hook <event>` directly - there is no forwarder script this module
//! owns, so own-entries are identified structurally, by [`is_managed_entry`], rather than by a
//! stable Jerry-owned directory the way the old forwarder-script scheme did: the located `jerry`
//! binary can move between Jerry versions (a relocated install, a different `PATH` resolution)
//! in a way a fixed marker directory cannot express, but "invokes a binary named
//! `jerry`/`jerry.exe` with `hook <event>`" cannot.

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

/// Timeout Jerry declares on each of its own hook entries, in seconds. `jerry hook` bounds its
/// own connected call to a few seconds (`jerry_cli`'s own `HOOK_CALL_TIMEOUT`) - this is a
/// generous ceiling so a slow or loaded machine's hook call is never itself the thing that trips
/// Cursor's own timeout handling.
const HOOK_TIMEOUT_SECS: u64 = 30;

/// The substring every one of Jerry's own managed commands contains right after the quoted
/// binary path - see [`is_managed_entry`].
const MANAGED_MARKERS: [&str; 2] = ["jerry' hook ", "jerry.exe' hook "];

/// `~/.cursor/hooks.json` - the one location `cursor-agent` actually reads a global hooks file
/// from. There is no CLI flag or environment variable that points it elsewhere (GitHub issue
/// #479's research comment).
pub fn hooks_json_path() -> Option<PathBuf> {
    Some(home_dir()?.join(".cursor").join("hooks.json"))
}

/// Merges Jerry's 6 managed entries into `cursor_hooks_json_path`, each running
/// `jerry_binary hook <event>` - see this module's own docs for the merge/identity/sweep rules.
/// Idempotent: running this twice in a row with the same `jerry_binary` writes nothing the
/// second time.
///
/// Never clobbers a file it cannot parse: if `cursor_hooks_json_path` exists but is not valid
/// JSON, or its root is not a JSON object, this returns `Ok(())` having touched nothing at all -
/// exactly [`crate::hooks::event::parse`]'s own "gracefully decline" shape, not an error, because
/// "the user's hand-edited file is currently broken" isn't a failure of *this* operation.
pub fn install(cursor_hooks_json_path: &Path, jerry_binary: &Path) -> io::Result<()> {
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

    for event in CURSOR_HOOK_EVENTS {
        let command = managed_command(jerry_binary, event)?;
        let entries = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(array) = entries.as_array_mut() else {
            return Ok(());
        };
        array.retain(|entry| !is_managed_entry(entry));
        array.push(serde_json::json!({ "command": command, "timeout": HOOK_TIMEOUT_SECS }));
    }

    write_if_changed(cursor_hooks_json_path, existing_raw.as_deref(), &root)
}

/// Strips every entry [`install`] would have written, under every one of [`CURSOR_HOOK_EVENTS`],
/// identified the same way [`install`] identifies them - dropping an event's own key entirely if
/// removing Jerry's entry leaves it empty, but never touching a key that still holds real
/// user-authored entries. Needs no `jerry_binary` (matching is structural, see the module docs),
/// so it works even when the binary can no longer be located. A complete no-op - it doesn't even
/// open the file - when `cursor_hooks_json_path` doesn't exist, and the same "abort untouched"
/// behaviour as [`install`] when it exists but isn't parseable.
pub fn remove_managed_entries(cursor_hooks_json_path: &Path) -> io::Result<()> {
    let Some((mut root, existing_raw)) = load_mergeable(cursor_hooks_json_path, false)? else {
        return Ok(());
    };
    let Some(object) = root.as_object_mut() else {
        return Ok(());
    };
    let Some(hooks_obj) = object.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };

    let mut now_empty = Vec::new();
    for event in CURSOR_HOOK_EVENTS {
        let Some(array) = hooks_obj.get_mut(event).and_then(Value::as_array_mut) else {
            continue;
        };
        array.retain(|entry| !is_managed_entry(entry));
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

/// Whether `entry`'s `command` field is one of Jerry's own: it invokes a binary named
/// `jerry`/`jerry.exe`, single-quoted (`shell_quote`/`powershell_quote`'s shared idiom), with
/// jerry's own `hook <event>` subcommand right after the closing quote. Checked structurally -
/// by the binary's own file name and the literal ` hook ` marker - rather than by a fixed parent
/// directory, since the located `jerry` binary can move between installs in a way a stable
/// Jerry-owned directory used to guarantee (see the module docs).
fn is_managed_entry(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| {
            MANAGED_MARKERS
                .iter()
                .any(|marker| command.contains(marker))
        })
}

/// The real `command` string for one event's managed entry - `<jerry_binary> hook <event>` on
/// POSIX (quoted via [`shell_quote`], reused verbatim from `crate::hooks::settings_file` rather
/// than re-implementing the same escaping), or the `powershell.exe ... -Command "& '<jerry_binary>'
/// hook <event>"` wrapper `crate::hooks::settings_file::windows_hook_entry` builds for Claude's
/// settings file - deliberately parallel, reusing [`powershell_quote`] rather than a second
/// quoter, since `hooks.json`'s `command` field is executed the same way a shell command line is.
fn managed_command(jerry_binary: &Path, event: &str) -> io::Result<String> {
    let binary = jerry_binary.to_string_lossy();
    if cfg!(windows) {
        let quoted = powershell_quote(&binary)?;
        Ok(format!(
            "powershell.exe -NoProfile -NonInteractive -Command \"& {quoted} hook {event}\""
        ))
    } else {
        Ok(format!("{} hook {event}", shell_quote(&binary)))
    }
}

#[cfg(test)]
mod cursor_hooks_file_tests {
    use std::path::{Path, PathBuf};

    use serde_json::Value;

    use super::{
        install, managed_command, remove_managed_entries, CURSOR_HOOK_EVENTS, HOOK_TIMEOUT_SECS,
    };

    fn jerry_binary(root: &Path) -> PathBuf {
        root.join(if cfg!(windows) { "jerry.exe" } else { "jerry" })
    }

    #[test]
    fn a_fresh_or_missing_file_gets_created_with_the_six_managed_entries_and_version_1() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let jerry = jerry_binary(temp.path());

        install(&hooks_json, &jerry).expect("install must succeed");

        let raw = std::fs::read_to_string(&hooks_json).expect("file must exist");
        let parsed: Value = serde_json::from_str(&raw).expect("must be valid JSON");
        assert_eq!(parsed["version"], 1);
        let hooks = parsed["hooks"].as_object().expect("a hooks object");
        assert_eq!(hooks.len(), CURSOR_HOOK_EVENTS.len());
        for event in CURSOR_HOOK_EVENTS {
            let entries = hooks[event].as_array().expect("an array");
            assert_eq!(entries.len(), 1, "{event}: exactly one managed entry");
            let command = entries[0]["command"].as_str().expect("a command string");
            assert!(command.contains(&jerry.to_string_lossy().into_owned()));
            assert!(
                command.ends_with(&format!(" hook {event}"))
                    || command.ends_with(&format!(" hook {event}\""))
            );
            assert_eq!(entries[0]["timeout"], HOOK_TIMEOUT_SECS);
        }
    }

    #[test]
    fn unrelated_user_authored_hooks_survive_byte_for_byte_alongside_managed_entries() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let jerry = jerry_binary(temp.path());

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

        install(&hooks_json, &jerry).expect("install must succeed");

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
            .is_some_and(|command| command.contains(&jerry.to_string_lossy().into_owned()))));
    }

    #[test]
    fn a_relocated_jerry_binary_replaces_the_old_entry_rather_than_accumulating() {
        // The structural marker means the sweep no longer depends on a stable Jerry-owned
        // directory: an entry that named a *different path* to a binary still named `jerry` (or
        // `jerry.exe`) must still be recognised as Jerry's own and replaced.
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let old_jerry =
            temp.path()
                .join("old-install")
                .join(if cfg!(windows) { "jerry.exe" } else { "jerry" });
        let new_jerry =
            temp.path()
                .join("new-install")
                .join(if cfg!(windows) { "jerry.exe" } else { "jerry" });

        install(&hooks_json, &old_jerry).expect("first install");
        install(&hooks_json, &new_jerry).expect("second install, relocated binary");

        let raw = std::fs::read_to_string(&hooks_json).expect("read");
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        for event in CURSOR_HOOK_EVENTS {
            let entries = parsed["hooks"][event].as_array().expect("an array");
            assert_eq!(
                entries.len(),
                1,
                "{event}: the old install's entry must be swept, not accumulated"
            );
            assert!(
                entries[0]["command"].as_str().is_some_and(
                    |command| command.contains(&new_jerry.to_string_lossy().into_owned())
                )
            );
        }
    }

    #[test]
    fn an_unrelated_tools_entry_under_a_different_binary_name_is_never_touched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let jerry = jerry_binary(temp.path());

        let original = serde_json::json!({
            "hooks": { "stop": [{ "command": "/opt/someone-elses/tool/hook.sh stop", "timeout": 5 }] }
        });
        std::fs::write(
            &hooks_json,
            serde_json::to_string_pretty(&original).expect("serialize"),
        )
        .expect("seed file");

        install(&hooks_json, &jerry).expect("install");
        remove_managed_entries(&hooks_json).expect("remove");

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
        let jerry = jerry_binary(temp.path());
        std::fs::write(&hooks_json, b"{ not valid json at all").expect("seed file");
        let before = std::fs::read(&hooks_json).expect("read before");

        let result = install(&hooks_json, &jerry);

        assert!(result.is_ok(), "must not surface a hard error: {result:?}");
        let after = std::fs::read(&hooks_json).expect("read after");
        assert_eq!(before, after, "an unparseable file must never be rewritten");
    }

    #[test]
    fn a_json_array_root_is_also_refused_rather_than_treated_as_mergeable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let jerry = jerry_binary(temp.path());
        std::fs::write(&hooks_json, b"[]").expect("seed file");

        install(&hooks_json, &jerry).expect("must not error");

        let after = std::fs::read(&hooks_json).expect("read after");
        assert_eq!(after, b"[]", "a non-object root must never be rewritten");
    }

    #[test]
    fn installing_twice_in_a_row_is_a_real_no_op_the_second_time() {
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        let jerry = jerry_binary(temp.path());

        install(&hooks_json, &jerry).expect("first install");
        let mtime_after_first = std::fs::metadata(&hooks_json)
            .expect("metadata")
            .modified()
            .expect("mtime");

        // A real, observable delay so a second write (if one happened) would produce a strictly
        // later mtime on every platform's filesystem timestamp resolution.
        std::thread::sleep(std::time::Duration::from_millis(20));
        install(&hooks_json, &jerry).expect("second install");
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
        let jerry = jerry_binary(temp.path());

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
        install(&hooks_json, &jerry).expect("install");

        remove_managed_entries(&hooks_json).expect("remove");

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

        remove_managed_entries(&hooks_json).expect("must not error");

        assert!(!hooks_json.exists(), "nothing must be created");
    }

    #[test]
    fn remove_managed_entries_needs_no_locatable_jerry_binary() {
        // The real reason matching went structural: a user who uninstalled the binary that wrote
        // an entry (or moved it) must still be able to turn the setting off and have Jerry's own
        // entries actually removed, with nothing left to locate.
        let temp = tempfile::tempdir().expect("temp dir");
        let hooks_json = temp.path().join("hooks.json");
        install(&hooks_json, &jerry_binary(temp.path())).expect("install");

        remove_managed_entries(&hooks_json).expect("remove needs no jerry_binary argument at all");

        let raw = std::fs::read_to_string(&hooks_json).expect("read");
        let parsed: Value = serde_json::from_str(&raw).expect("valid JSON");
        assert!(parsed["hooks"].as_object().expect("hooks").is_empty());
    }

    #[test]
    fn the_managed_command_carries_the_event_name_and_the_jerry_path() {
        let jerry = PathBuf::from(if cfg!(windows) {
            r"C:\Users\me\AppData\Local\Jerry\jerry.exe"
        } else {
            "/home/user/.local/bin/jerry"
        });
        for event in CURSOR_HOOK_EVENTS {
            let command = managed_command(&jerry, event).expect("must build");
            if cfg!(windows) {
                assert!(command.starts_with("powershell.exe "));
            } else {
                assert!(command.contains(&jerry.to_string_lossy().into_owned()));
            }
            assert!(command.contains(&format!(" hook {event}")));
        }
    }

    #[test]
    fn the_managed_command_is_recognised_as_jerry_s_own_by_the_real_matcher() {
        let jerry = jerry_binary(Path::new("/wherever"));
        let command = managed_command(&jerry, "stop").expect("must build");
        assert!(
            super::is_managed_entry(&serde_json::json!({ "command": command, "timeout": 5 })),
            "a freshly generated command must match its own recogniser: {command:?}"
        );
    }
}
