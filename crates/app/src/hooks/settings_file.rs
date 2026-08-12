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
//!
//! ## Two forwarders: POSIX `sh` and Windows PowerShell
//!
//! Both properties above are a *contract*, not a script - so there are two scripts honouring it,
//! [`UNIX_FORWARDER_SCRIPT`] and [`WINDOWS_FORWARDER_SCRIPT`], and the platform picks one. Native
//! Windows was originally left out of hook injection entirely (`is_supported()` was `cfg!(unix)`),
//! which meant every Windows user silently got only the Phase 1 title/OSC and quiescence
//! heuristics no matter what Claude Code was actually reporting. That was a real, load-bearing
//! gap, not a cosmetic one, and it is what this half of the module closes.
//!
//! ### Which shell runs the `command`, and why that had to be pinned
//!
//! Claude Code's own hook documentation (<https://code.claude.com/docs/en/hooks>) is explicit
//! that a `command` hook with no `args` is *shell form*: the string is handed to `sh -c` on
//! macOS/Linux, and on Windows to **"Git Bash, or PowerShell if Git Bash isn't installed"**. Two
//! possible shells with two different quoting languages is not something a generated command
//! string can be quietly correct under, so Jerry pins it with the documented `shell` field, which
//! "Accepts `"bash"` or `"powershell"`" and, set to `"powershell"`, "runs the command via
//! PowerShell on Windows".
//!
//! That the field is really honoured - and, more to the point, that adding it does not make a
//! real `claude` reject the whole settings file - was checked against the actual binary (2.1.228)
//! rather than taken from the docs: a settings file declaring three `SessionStart` hooks, one
//! plain shell-form, one with `"shell": "bash"`, and one exec-form with `args`, was run through a
//! real session, and all three fired.
//!
//! Exec form (`args`, no shell at all) was the other candidate and is genuinely tempting - it
//! removes the quoting problem outright. It was rejected on its *failure* mode. Exec form needs
//! `command` to name a real executable, so it would have to be `powershell.exe` with the script
//! path moved into `args`; a Claude Code that did not understand `args` would then fall back to
//! shell form and run a bare `powershell.exe` with the hook payload on its stdin - i.e. hand
//! model-authored JSON to a shell as a script to execute. Shell form's failure mode is a hook
//! that doesn't fire. That asymmetry decided it.
//!
//! For the same "be wrong safely" reason the generated string is deliberately written so that it
//! is *also* a valid Git Bash command line, in case a Claude Code release ever ignores `shell`:
//! `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File '<path>' <Event>`
//! parses identically in PowerShell's argument mode and in `bash`, because both treat a
//! single-quoted token as a wholly literal string. See [`powershell_quote`] for the one character
//! where the two disagree and why that disagreement is safe.
//!
//! ### Why the Windows forwarder is a `.ps1` invoked through a second `powershell.exe`
//!
//! A `.ps1` cannot simply be `&`-invoked from the shell Claude Code already started: PowerShell's
//! execution policy governs running script *files*, and the default on Windows client SKUs is
//! `Restricted`, so the script would be refused and hooks would silently never fire - precisely
//! the bug class this change exists to fix. `-ExecutionPolicy Bypass -File` is the documented way
//! to run one script without changing any machine state, and it costs one extra process launch
//! per hook. The alternative that avoids that launch - reading the file and running it through
//! `[ScriptBlock]::Create` - is the textbook execution-policy-evasion pattern that endpoint
//! protection flags, which is a worse trade for a tool that has to just work on a corporate
//! laptop.
//!
//! A `.cmd`/`.bat` forwarder would start faster than PowerShell, and was considered for that
//! reason, but batch expands `%JERRY_HOOK_TOKEN%` *textually into a command line*: the class of
//! bug where a value containing `&` or `"` stops being data and becomes another command. The
//! PowerShell forwarder interpolates the same value into a real argument vector instead, which
//! cannot do that at all.
//!
//! ### Why the Windows forwarder spools stdin to a file instead of piping it
//!
//! The `sh` forwarder hands its own stdin straight to `curl --data-binary @-`. The PowerShell one
//! copies stdin to a temporary file in the launch directory and passes `--data-binary @<file>`,
//! deleting it immediately afterwards. That is not caution about PowerShell's pipeline for its
//! own sake - it is that every text-shaped route is actively wrong on Windows: `[Console]::In`
//! decodes stdin using the console code page (OEM 437 on a default English install) and piping a
//! string into a native command re-encodes it with `$OutputEncoding`, which is ASCII in Windows
//! PowerShell 5.1. Either one silently mangles every non-ASCII character in the payload. A raw
//! `Stream.CopyTo` involves no encoding at all, and it also cuts the number of processes that
//! have to inherit the payload's stdin handle from two to one.
//!
//! The spool file holds the same hook payload that is about to be POSTed and never the token; it
//! is written inside the already-private launch directory and removed in a `finally` block, and
//! [`HookFiles::drop`]/[`sweep_stale_directories`] remove the whole directory regardless.

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
///
/// `$1` is the event name, supplied per hook entry by the generated settings file.
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
///
/// The event name arrives as the single positional argument `powershell.exe -File <this> <Event>`
/// passes through, which is the `$1` of the `sh` script.
///
/// `$JerryHookEvent`, not `$Event`: `$Event` is a PowerShell *automatic* variable (the eventing
/// subsystem's), and a `param()` that shadows an automatic variable is a real footgun rather than
/// a style point.
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

$JerryPayload = $null
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

    # Byte-for-byte, with no text decoding anywhere - see the module docs for why every
    # string-shaped route through PowerShell corrupts non-ASCII payloads on Windows.
    $JerryPayload = Join-Path -Path $PSScriptRoot -ChildPath ('payload-' + [System.Guid]::NewGuid().ToString('N') + '.json')
    $JerryStdin = [Console]::OpenStandardInput()
    $JerrySpool = [System.IO.File]::Open($JerryPayload, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    try { $JerryStdin.CopyTo($JerrySpool) } finally { $JerrySpool.Dispose() }

    # Every one of these is a real element of curl's argument vector, so a value containing a
    # space, a quote or an ampersand is data and can never become another command.
    $JerryArgs = @(
        '--silent',
        '--max-time', '5',
        '--request', 'POST',
        '--header', "Authorization: Bearer $($env:JERRY_HOOK_TOKEN)",
        '--header', 'Content-Type: application/json',
        '--data-binary', "@$JerryPayload",
        "http://127.0.0.1:$($env:JERRY_HOOK_PORT)/hook?event=$JerryHookEvent&agent=$($env:JERRY_AGENT_ID)"
    )
    # Never propagate curl's exit status, and never let its output reach Claude Code: stdout on
    # some events is fed back to the model as context.
    & $JerryCurl @JerryArgs 2>$null | Out-Null
} catch {
    # Deliberately swallowed. A dead listener, a vanished launch directory or a curl that will not
    # start must all cost exactly nothing.
} finally {
    if ($JerryPayload) { Remove-Item -LiteralPath $JerryPayload -Force -ErrorAction SilentlyContinue }
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
        // Executable, but only by this user. (Windows has no execute bit and does not need one -
        // the script is run as an argument to `powershell.exe -File`, never by exec'ing the file
        // itself; see [`write_private_file`].)
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
    ///
    /// Best-effort is load-bearing rather than merely tolerant on Windows, where an open file
    /// blocks the removal of its directory: a hook that is in flight at the moment Jerry quits
    /// holds its spooled payload open, and this then fails. That is the intended outcome, not a
    /// leak - the directory is left for [`sweep_stale_directories`] to collect on the next launch,
    /// by which point this instance's pid is genuinely dead.
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

/// `CreateDirectoryW(path)`, failing if anything already exists at `path` - the Windows half of
/// [`create_private_dir`]'s three properties, and a real code path now that [`is_supported`] is
/// true here.
///
/// **Exclusivity carries over exactly.** `DirBuilder::create` is non-recursive, so it is a single
/// `CreateDirectoryW`, which fails with `ERROR_ALREADY_EXISTS` if *anything* is at the path -
/// including a directory symlink or any other reparse point. That is the same guarantee the Unix
/// path's `create`-not-`recursive(true)` gives, and it is what stops a pre-planted path from being
/// adopted. The unpredictable random suffix carries over unchanged too.
///
/// **The `0o700` has no Windows spelling, and does not need one here.** There is no mode argument
/// to `CreateDirectoryW`; access is decided by the DACL, which - with a null
/// `SECURITY_ATTRIBUTES`, as `std` passes - is inherited from the parent directory. In production
/// the parent is `std::env::temp_dir()`, i.e. `GetTempPath2W`, i.e. `%TMP%`/`%TEMP%`, which by
/// default is the *per-user* `%LOCALAPPDATA%\Temp`. That directory's default ACL grants full
/// control to the owning user, `SYSTEM` and `Administrators`, and nothing to other interactive
/// users, so the inherited result is the practical equivalent of `0o700`: another logged-in
/// non-administrator cannot read or write inside it. Windows has no world-writable `/tmp`
/// equivalent in the default configuration, which is the specific hazard `0o700` exists to answer.
///
/// **The residual gap, stated rather than papered over.** If `%TEMP%` has been redirected to a
/// permissively-ACL'd shared location, the inherited DACL is whatever that location grants, and a
/// local attacker who won the race against the 64-bit random suffix could overwrite the forwarder
/// script and get it run by Claude Code as this user. Writing an explicit DACL would need
/// `CreateDirectoryW` with a hand-built `SECURITY_ATTRIBUTES` through raw FFI, which is a
/// materially larger `unsafe` surface than this module carries anywhere else; it is a named,
/// real follow-up rather than something quietly assumed away. Note the files themselves still
/// hold no secret in that scenario - the token is only ever in this process's memory and in its
/// children's environments (see [`HookFiles::drop`]).
#[cfg(windows)]
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
///
/// `mode` is a no-op on Windows, and deliberately so rather than for want of an equivalent:
///
/// - The *access* half is answered by the containing directory, exactly as
///   [`new_dir_owner_only`]'s Windows twin documents - `create_new` maps to `CREATE_NEW`, so the
///   exclusivity that stops a pre-planted path being written through carries over unchanged, and
///   a null `SECURITY_ATTRIBUTES` means the new file inherits the launch directory's DACL.
/// - The *execute* half simply does not exist. The whole reason the Unix path re-`fchmod`s after
///   `open` is that a umask could strip `0o700` down to `0o600`, leave the script non-executable,
///   and make Claude Code's invocation exit 126 with hooks silently never firing. Windows has no
///   execute bit, and the Windows forwarder is never exec'd anyway: it is passed as an *argument*
///   to `powershell.exe -File`, which only needs to be able to read it. There is nothing here
///   that a mode could get wrong.
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

/// Whether a process with this id currently exists - the Windows twin of the `kill(pid, 0)` check
/// above.
///
/// This used to be a hardcoded `true`, which was honest while hook injection was disabled on
/// Windows (nothing was ever written, so there was nothing to sweep) and is not any more: it would
/// now mean every `SIGKILL`-equivalent of a Jerry - Task Manager's "End task", a crash, a power
/// loss - leaks its launch directory into `%TEMP%` forever.
///
/// Two Win32 calls rather than one, because the obvious single-call version is wrong:
///
/// - `OpenProcess` failing with `ERROR_INVALID_PARAMETER` is the real "no such process" answer.
///   `ERROR_ACCESS_DENIED` means the opposite - the process exists, it just belongs to another
///   user or is more privileged - and is reported as alive, the same call the Unix path makes for
///   `EPERM`, and for the same reason: "not mine" must never be read as "dead, delete its files".
///   Any other failure is also treated as alive, because this is tidying and a wrong "dead" is the
///   only answer here that destroys anything.
/// - A successful `OpenProcess` is *not* on its own proof of life. A process that has exited but
///   still has an open handle somewhere remains openable by pid, so liveness is decided by
///   `WaitForSingleObject(handle, 0)`: the handle is signalled once the process terminates, so
///   `WAIT_TIMEOUT` means still running and `WAIT_OBJECT_0` means exited. `GetExitCodeProcess` was
///   the alternative and is subtly broken - it reports `STILL_ACTIVE` (259) for a live process and
///   for a dead one that genuinely exited with code 259.
///
/// The access mask is `PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE`, and both halves are
/// needed. `SYNCHRONIZE` is the right `WaitForSingleObject` actually requires - without it the
/// wait fails rather than reporting liveness, which would make every process look dead and every
/// directory sweepable. `PROCESS_QUERY_LIMITED_INFORMATION` is the *weakest* query right, chosen
/// over `PROCESS_QUERY_INFORMATION` because it is grantable across integrity levels, so an
/// unelevated Jerry can still tell that an elevated (or another user's) process is alive rather
/// than falling into the error path. `windows-sys` exposes `SYNCHRONIZE` only under
/// `Win32::Storage::FileSystem` - it is one of the standard access rights shared by every kind of
/// securable object, and that module is simply where the generated bindings happen to put it; both
/// constants are plain `u32`, so the mask composes as written.
///
/// Windows recycles pids aggressively, so a swept-too-early directory is the failure worth
/// designing against, and recycling can only push this the safe way: a reused pid belongs to a
/// live process, which reads as alive and simply leaves the directory for a later sweep.
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, WAIT_TIMEOUT};
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
    state == WAIT_TIMEOUT
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
/// Unix *and* Windows. Both have a real forwarder ([`UNIX_FORWARDER_SCRIPT`],
/// [`WINDOWS_FORWARDER_SCRIPT`]) and a real, deliberately-pinned shell for Claude Code to run the
/// generated `command` through - see the module docs for the whole Windows design and for what in
/// it was verified against a real `claude` versus reasoned through.
///
/// This is not `true`. Anything that is neither Unix nor Windows - a `wasm32` target, say - has no
/// forwarder written for it, and gets the honest answer: hook injection is skipped and every agent
/// falls back to the Phase 1 title/OSC and quiescence signals, which work identically everywhere.
/// The graceful fallback in [`crate::hooks::HookRuntime::start`] still covers the real per-machine
/// failures (an unwritable temp directory, a loopback port that will not bind) on every supported
/// platform alike.
pub const fn is_supported() -> bool {
    cfg!(unix) || cfg!(windows)
}

/// Builds the real `--settings` JSON declaring every [`FORWARDED_EVENTS`] entry against
/// `forwarder`.
///
/// Built with `serde_json` rather than string formatting so the script's path is escaped
/// correctly no matter what it contains. The path is additionally quoted *within* the shell
/// command string, because Claude Code runs a `command` hook through a shell - an unquoted path
/// containing a space would otherwise be split into a wrong program plus stray arguments. Which
/// shell, and therefore which quoting language, is [`unix_hook_entry`] versus
/// [`windows_hook_entry`].
fn settings_json(forwarder: &Path) -> io::Result<String> {
    let forwarder = forwarder.to_string_lossy();
    let mut hooks = serde_json::Map::new();
    for event in FORWARDED_EVENTS {
        hooks.insert(
            event.to_string(),
            serde_json::json!([{ "hooks": [hook_entry(&forwarder, event)] }]),
        );
    }
    let document = serde_json::json!({ "hooks": serde_json::Value::Object(hooks) });
    serde_json::to_string_pretty(&document)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// This platform's hook entry.
#[cfg(not(windows))]
fn hook_entry(forwarder: &str, event: &str) -> serde_json::Value {
    unix_hook_entry(forwarder, event)
}

/// This platform's hook entry.
#[cfg(windows)]
fn hook_entry(forwarder: &str, event: &str) -> serde_json::Value {
    windows_hook_entry(forwarder, event)
}

/// One `"type": "command"` entry running the POSIX forwarder, for the `sh -c` Claude Code uses on
/// macOS and Linux.
///
/// Compiled on every platform even though it is only *called* on Unix, so the Windows suite runs
/// it too rather than only type-checking it - the same call
/// `crate::settings::state::windows_shell_suggestions` already makes in the other direction, and
/// the only way a machine that can run one of these can test the other's string generation at all.
pub fn unix_hook_entry(forwarder: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        "command": format!("{} {event}", shell_quote(forwarder)),
    })
}

/// One `"type": "command"` entry running the PowerShell forwarder on Windows - see the module docs
/// for why the shell is pinned, why the invocation goes through a second `powershell.exe`, and why
/// the resulting string is deliberately also valid `bash`.
///
/// Compiled on every platform for the same reason as [`unix_hook_entry`].
pub fn windows_hook_entry(forwarder: &str, event: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "command",
        // Documented Claude Code field, accepting exactly "bash" or "powershell". Without it the
        // shell on Windows is "Git Bash, or PowerShell if Git Bash isn't installed" - two quoting
        // languages, decided by what the user happens to have installed.
        "shell": "powershell",
        "command": format!(
            "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {} {event}",
            powershell_quote(forwarder)
        ),
    })
}

/// Wraps `value` in POSIX single quotes, escaping any single quote inside it via the standard
/// `'\''` idiom. Single quotes are used rather than double because inside them the shell expands
/// nothing at all - so a path containing `$`, backticks or `\` is passed through literally.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Wraps `value` in PowerShell single quotes, escaping any single quote inside it by doubling it.
///
/// PowerShell's single-quoted string is the exact analogue of `sh`'s: nothing inside it is
/// expanded, so `$env:PATH`, a backtick, `$(...)`, `;`, `&`, `|` and a Windows path's backslashes
/// are all literal, and `''` is the language's own way of writing one literal quote. There is no
/// escape sequence *inside* a single-quoted string that can end it early - the closing quote is
/// the only thing that ends it, and a doubled quote is by definition not that - so no `value`,
/// however hostile, can reach the surrounding command line. That is the whole security property,
/// and it is what [`a_hostile_path_cannot_break_out_of_the_generated_windows_command`] pins.
///
/// This is used in PowerShell's *argument* mode, not expression mode: the generated command starts
/// with the bare word `powershell.exe`, which puts the parser in command mode for the rest of the
/// line, where a token beginning with `'` is parsed as a literal string argument.
///
/// **The one divergence from `bash`, and why it is safe.** The module docs explain that the
/// generated string is also valid `bash`, as insurance against a Claude Code that ignores the
/// `shell` field. Single quotes behave identically in both - except for the escape: `bash` writes
/// a literal quote as `'\''` and PowerShell as `''`. A path containing an apostrophe
/// (`C:\Users\O'Brien\...` - legal, and a real surname) therefore round-trips correctly under
/// PowerShell and, under a `bash` fallback, collapses to a path with the apostrophe removed. That
/// is a file that does not exist, so the hook does not fire and the rail falls back to the Phase 1
/// signals. It is *not* a quoting escape: `bash` also treats the doubled quote as string
/// concatenation, never as an end to quoting followed by live text. Wrong, visibly, in the safe
/// direction, only on a Claude Code old enough to ignore a documented field, only for a user whose
/// temp path contains an apostrophe.
///
/// Windows filenames cannot contain `"` at all (it is one of the reserved characters, alongside
/// `<>:|?*`), so the double-quote case a `cmd.exe`-style quoter would have to agonise over cannot
/// arise from a real path. The single-quoted form is used regardless, rather than relying on that,
/// because `value` reaching here is a `to_string_lossy` of an arbitrary `PathBuf`.
pub fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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

    // ---------------------------------------------------------------------------------------
    // The Windows half.
    //
    // Everything from here to `windows_only` is deliberately *not* `#[cfg(windows)]`: it is pure
    // string generation, so it is exactly as testable on Linux as on Windows, and gating it would
    // leave the Windows quoting - the single place where getting it wrong means "hooks silently
    // never fire", which is the bug this whole path exists to fix - covered by no suite anybody
    // routinely runs. Same call `crate::settings::state::windows_shell_suggestions` already makes.
    // ---------------------------------------------------------------------------------------

    #[test]
    fn a_windows_path_with_powershell_metacharacters_is_quoted_rather_than_expanded() {
        assert_eq!(
            powershell_quote(r"C:\Temp\plain"),
            r"'C:\Temp\plain'",
            "backslashes are literal inside single quotes in both PowerShell and bash"
        );
        assert_eq!(
            powershell_quote(r"C:\Program Files (x86)\jerry"),
            r"'C:\Program Files (x86)\jerry'",
            "the single most common real Windows path shape: spaces and parentheses"
        );
        assert_eq!(
            powershell_quote(r"C:\Temp\$(Get-Content secret)"),
            r"'C:\Temp\$(Get-Content secret)'",
            "a subexpression must stay literal - this is the injection case"
        );
        assert_eq!(
            powershell_quote("C:\\Temp\\`whoami`"),
            "'C:\\Temp\\`whoami`'",
            "a backtick is PowerShell's escape character *outside* single quotes only"
        );
        assert_eq!(
            powershell_quote(r"C:\Users\O'Brien\jerry"),
            r"'C:\Users\O''Brien\jerry'",
            "an apostrophe is doubled - PowerShell's own escape, see `powershell_quote`'s docs"
        );
        assert_eq!(
            powershell_quote(r"C:\Temp\a&b;c|d"),
            r"'C:\Temp\a&b;c|d'",
            "command separators must stay literal"
        );
    }

    #[test]
    fn a_hostile_path_cannot_break_out_of_the_generated_windows_command() {
        // The adversarial case. If a temp path could end the quoted region early, everything after
        // it becomes live PowerShell running as the user, fired by Claude Code on every tool call.
        // The property that makes that impossible is that a single-quoted PowerShell string has no
        // internal escape sequence at all: only an *odd* run of quotes can end it, and doubling
        // guarantees every run is even.
        let hostile = r"C:\Temp\x'; Start-Process calc.exe; '";
        let entry = windows_hook_entry(hostile, "Stop");
        let command = entry["command"].as_str().expect("a command string");

        let quoted = powershell_quote(hostile);
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
                windows_hook_entry(r"C:\Temp\jerry-hooks-1-ab\jerry-hook-forwarder.ps1", event);
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
        let entry = windows_hook_entry(forwarder, "PreToolUse");
        let command = entry["command"].as_str().expect("a command");

        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("PATH", &bin)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("the generated command must be runnable by a real POSIX shell");
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
            script.contains("--data-binary"),
            "the payload must be posted verbatim, exactly as the `sh` forwarder does"
        );
        assert!(
            script.contains("Out-Null"),
            "curl's output must never reach Claude Code - it is fed back to the model as context \
             on some events"
        );
        // `$Event` is a PowerShell automatic variable; shadowing it in `param()` is a real footgun.
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
            // The only place a literal token could plausibly be spliced is a `Bearer` header.
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
    ///
    /// These cannot run on the Linux machines this suite is usually run on - there is no
    /// PowerShell and no `OpenProcess` - so they are `#[cfg(windows)]` rather than skipped at run
    /// time, and they are written to be run for real in Windows CI or on a developer's Windows
    /// machine. Everything above this module is the part that *can* be checked anywhere, and it
    /// deliberately covers all of the string generation.
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
        fn the_forwarder_leaves_no_spooled_payload_behind() {
            // The Windows forwarder writes stdin to a temporary file in the launch directory (see
            // the module docs). It must delete it - the launch directory is swept only when Jerry
            // restarts, and a long-lived window would otherwise accumulate one file per hook.
            let temp = tempfile::tempdir().expect("temp dir");
            let files = HookFiles::write_in(temp.path()).expect("must write");
            let directory = files
                .settings_path()
                .parent()
                .expect("parent")
                .to_path_buf();
            let dead_port = {
                let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
                listener.local_addr().expect("addr").port()
            };
            let output = generated_stop_command(&files)
                .env(PORT_ENV, dead_port.to_string())
                .env(TOKEN_ENV, "irrelevant")
                .env(AGENT_ENV, "1")
                .stdin(std::process::Stdio::null())
                .output()
                .expect("the generated command must run");
            assert!(output.status.success());

            let leftovers: Vec<_> = std::fs::read_dir(&directory)
                .expect("read launch dir")
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("payload-"))
                .collect();
            assert!(
                leftovers.is_empty(),
                "the spooled payload must be removed even when the POST fails: {leftovers:?}"
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
            // The Windows twin of the `sweep_stale_directories` test above, keyed on a real pid.
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
