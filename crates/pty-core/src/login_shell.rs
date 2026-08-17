//! Resolves the user's real login-shell `PATH` when this process was launched by something that
//! hands it a minimal one (macOS Finder / launchd, a Linux `.desktop` entry) rather than a
//! terminal. Unix only - Windows processes inherit the same PATH a terminal would use, so the
//! problem this exists to fix does not occur there.
//!
//! `crates/app/src/main.rs` is the only intended caller: it merges the result into the real
//! process environment before `app::run`, so every spawned agent PTY inherits it too.

use std::collections::HashSet;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long the login-shell probe waits before giving up and falling back to the inherited
/// PATH. A misbehaving rc file (one that blocks on a prompt) must never hang app startup.
const LOGIN_SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Poll interval while waiting out [`LOGIN_SHELL_PROBE_TIMEOUT`].
const LOGIN_SHELL_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The PATH launchd (macOS) or a `.desktop` launch (Linux) hands a freshly spawned process,
/// before any shell rc file has run.
const MINIMAL_LAUNCHER_PATH_ENTRIES: [&str; 4] = ["/usr/bin", "/bin", "/usr/sbin", "/sbin"];

/// Resolves the PATH a GUI-launched process (Finder's `.app`, a `.desktop` entry) is missing:
/// the one the user's real login shell produces after sourcing their rc files, merged with
/// whatever PATH this process already inherited.
///
/// A deliberate no-op - returns `None`, spawning nothing - when `inherited` already looks like a
/// real shell PATH, or standard input is a tty (this process was itself launched from a
/// terminal, where the inherited PATH is already correct): paying a shell spawn on every
/// terminal launch would be wasted latency for no behavior change.
///
/// Falls back to `None` genuinely, never a guessed result, when `$SHELL` is unset, names a shell
/// this cannot query (see [`login_shell_probe_args`]), or the probe fails, produces no usable
/// output, or exceeds [`LOGIN_SHELL_PROBE_TIMEOUT`]. Callers keep using `inherited` as-is.
pub fn resolve_login_shell_path(inherited: &OsStr) -> Option<OsString> {
    if !path_looks_minimal(inherited) || std::io::stdin().is_terminal() {
        return None;
    }

    let shell = PathBuf::from(env::var_os("SHELL")?);
    let args = login_shell_probe_args(&shell)?;
    let raw_output = run_login_shell_probe(&shell, &args, LOGIN_SHELL_PROBE_TIMEOUT)?;
    let text = String::from_utf8_lossy(&raw_output);
    let probed_path = extract_last_nonempty_line(&text)?;
    if probed_path.is_empty() {
        return None;
    }

    Some(merge_path_entries(inherited, OsStr::new(probed_path)))
}

/// Whether `path` looks like [`MINIMAL_LAUNCHER_PATH_ENTRIES`] - worth probing - rather than an
/// already-real shell PATH, where probing would just be a wasted spawn for the same answer.
fn path_looks_minimal(path: &OsStr) -> bool {
    // `split_paths("")` yields one empty entry rather than none; dropping it makes an unset/empty
    // PATH read as minimal (so it gets probed) instead of accidentally as rich.
    env::split_paths(path)
        .filter(|entry| !entry.as_os_str().is_empty())
        .all(|entry| {
            MINIMAL_LAUNCHER_PATH_ENTRIES
                .iter()
                .any(|known| Path::new(known) == entry)
        })
}

/// The invocation that makes `shell` print its real, rc-file-sourced `PATH` as its last line of
/// output. `-l` (login) and `-i` (interactive) together cover where PATH mutation actually
/// happens across `.zprofile`/`.zshrc`/`.bash_profile`/`.bashrc` - most shells only source the
/// interactive-only rc file when both are set.
///
/// `fish` uses a different scripting syntax and stores `$PATH` as a list rather than a
/// colon-joined string, so `echo $PATH` there would print space-joined entries; `string join`
/// produces the colon-joined form every other caller in this crate expects. Shells this cannot
/// express a one-line probe for (nushell's `$env.PATH` access has no equivalent) return `None`
/// so the caller falls back to the inherited PATH instead of guessing.
fn login_shell_probe_args(shell: &Path) -> Option<Vec<&'static str>> {
    match shell.file_name().and_then(|name| name.to_str())? {
        "fish" => Some(vec!["-l", "-c", "string join : $PATH"]),
        "nu" | "nushell" => None,
        _ => Some(vec!["-l", "-i", "-c", "echo $PATH"]),
    }
}

/// Runs `shell args...` with stdin closed (so an rc file that reads input can't block on a
/// prompt) and a hard timeout, returning its raw stdout. `None` on any failure to spawn or
/// produce output before the timeout - genuinely unable to resolve, never a guessed result.
fn run_login_shell_probe(shell: &Path, args: &[&str], timeout: Duration) -> Option<Vec<u8>> {
    let mut child = Command::new(shell)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let (output_tx, output_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = output_tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) if Instant::now() >= deadline => {
                // Kill outright rather than a politer signal: this branch exists specifically
                // for a stuck rc file (blocked on a prompt), so nothing graceful is listening.
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            Ok(None) => std::thread::sleep(LOGIN_SHELL_PROBE_POLL_INTERVAL),
            Err(_) => return None,
        }
    }

    // Killing the shell does not necessarily close the stdout pipe: an rc file that backgrounds
    // anything (a daemon, a `& disown`ed helper) leaves a grandchild holding the inherited write
    // end open, so `read_to_end` keeps blocking and a plain `recv()` here would hang startup
    // forever - the exact failure this function's timeout exists to prevent. Bound the wait too,
    // and abandon the reader thread to its own eventual EOF if it comes to that.
    //
    // Bounded by the same deadline, not a second full `timeout`, so the whole probe costs what
    // this function promises rather than twice it. That distinction is the common case, not an
    // exotic one: `dash` (Linux's `/bin/sh`) forks the command it runs instead of `exec`ing it,
    // so killing the shell routinely leaves a live grandchild on the pipe. One poll interval of
    // floor keeps output a shell printed just before it hung from being dropped on a race.
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(LOGIN_SHELL_PROBE_POLL_INTERVAL);
    output_rx.recv_timeout(remaining).ok()
}

/// Takes the last non-empty line of `output`, so `-i`'s rc-file banners/MOTD noise printed
/// before the `echo $PATH` line isn't mistaken for the PATH itself - a login shell's PATH output
/// is always the final line, since it's the shell's last command.
fn extract_last_nonempty_line(output: &str) -> Option<&str> {
    output
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty())
}

/// Merges the probed login-shell PATH with the process's already-inherited PATH, deduping by
/// value while preserving order: probed entries first (matching the order a real terminal
/// session would resolve against), then any inherited-only entries appended after, so nothing
/// the caller already relied on is lost even if the login shell's PATH doesn't include it.
fn merge_path_entries(inherited: &OsStr, probed: &OsStr) -> OsString {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();
    // `split_paths("")` yields one empty `PathBuf` rather than zero entries; skip it so an
    // empty side never introduces a leading/spurious `:` (which POSIX PATH would read as "the
    // current directory" - never an intended outcome here).
    for entry in env::split_paths(probed)
        .chain(env::split_paths(inherited))
        .filter(|entry| !entry.as_os_str().is_empty())
    {
        if seen.insert(entry.clone()) {
            merged.push(entry);
        }
    }
    // Only fails if an entry itself contains the platform's path separator, which none of
    // `probed`/`inherited` can have introduced beyond what was already a valid PATH.
    env::join_paths(merged).unwrap_or_else(|_| inherited.to_os_string())
}

#[cfg(test)]
mod extract_last_nonempty_line_tests {
    use super::extract_last_nonempty_line;

    #[test]
    fn takes_the_last_line_ignoring_rc_file_banner_noise_above_it() {
        let output = "Welcome to zsh\nsome motd line\n/usr/bin:/bin:/opt/homebrew/bin\n";
        assert_eq!(
            extract_last_nonempty_line(output),
            Some("/usr/bin:/bin:/opt/homebrew/bin")
        );
    }

    #[test]
    fn trims_surrounding_whitespace_on_the_chosen_line() {
        let output = "banner\n  /usr/bin:/bin  \n";
        assert_eq!(extract_last_nonempty_line(output), Some("/usr/bin:/bin"));
    }

    #[test]
    fn skips_trailing_blank_lines_to_find_the_last_real_one() {
        let output = "/usr/bin:/bin\n\n\n";
        assert_eq!(extract_last_nonempty_line(output), Some("/usr/bin:/bin"));
    }

    #[test]
    fn returns_none_for_completely_empty_output() {
        assert_eq!(extract_last_nonempty_line(""), None);
    }

    #[test]
    fn returns_none_for_output_that_is_only_whitespace() {
        assert_eq!(extract_last_nonempty_line("  \n\n   \n"), None);
    }
}

#[cfg(test)]
mod path_looks_minimal_tests {
    use super::path_looks_minimal;
    use std::ffi::OsStr;

    #[test]
    fn the_exact_launchd_minimal_path_looks_minimal() {
        assert!(path_looks_minimal(OsStr::new(
            "/usr/bin:/bin:/usr/sbin:/sbin"
        )));
    }

    #[test]
    fn a_subset_of_the_minimal_entries_still_looks_minimal() {
        assert!(path_looks_minimal(OsStr::new("/usr/bin:/bin")));
    }

    #[test]
    fn a_path_with_a_homebrew_entry_does_not_look_minimal() {
        assert!(!path_looks_minimal(OsStr::new(
            "/usr/bin:/bin:/opt/homebrew/bin"
        )));
    }

    #[test]
    fn a_typical_rich_terminal_path_does_not_look_minimal() {
        assert!(!path_looks_minimal(OsStr::new(
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        )));
    }

    #[test]
    fn an_empty_path_looks_minimal_rather_than_rich() {
        // `split_paths("")` yields one *empty* entry, which matches none of the known minimal
        // ones - so without filtering it out this would read as a rich PATH and skip the probe.
        assert!(path_looks_minimal(OsStr::new("")));
    }
}

#[cfg(test)]
mod login_shell_probe_args_tests {
    use super::login_shell_probe_args;
    use std::path::Path;

    #[test]
    fn generic_posix_shells_get_a_plain_echo_path_probe() {
        assert_eq!(
            login_shell_probe_args(Path::new("/bin/zsh")),
            Some(vec!["-l", "-i", "-c", "echo $PATH"])
        );
        assert_eq!(
            login_shell_probe_args(Path::new("/bin/bash")),
            Some(vec!["-l", "-i", "-c", "echo $PATH"])
        );
    }

    #[test]
    fn fish_gets_a_colon_joining_probe_since_its_path_is_a_list() {
        assert_eq!(
            login_shell_probe_args(Path::new("/opt/homebrew/bin/fish")),
            Some(vec!["-l", "-c", "string join : $PATH"])
        );
    }

    #[test]
    fn nushell_has_no_expressible_probe_and_falls_back() {
        assert_eq!(login_shell_probe_args(Path::new("/usr/bin/nu")), None);
    }
}

#[cfg(test)]
mod merge_path_entries_tests {
    use super::merge_path_entries;
    use std::ffi::OsStr;

    #[test]
    fn probed_entries_come_first_with_inherited_only_entries_appended() {
        let inherited = OsStr::new("/usr/bin:/bin");
        let probed = OsStr::new("/opt/homebrew/bin:/usr/bin");

        let merged = merge_path_entries(inherited, probed);

        assert_eq!(merged.to_str(), Some("/opt/homebrew/bin:/usr/bin:/bin"));
    }

    #[test]
    fn duplicate_entries_across_both_sides_are_deduped() {
        let inherited = OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin");
        let probed = OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin");

        let merged = merge_path_entries(inherited, probed);

        assert_eq!(merged.to_str(), Some("/usr/bin:/bin:/usr/sbin:/sbin"));
    }

    #[test]
    fn an_empty_probed_path_still_yields_the_inherited_entries() {
        let inherited = OsStr::new("/usr/bin:/bin");
        let probed = OsStr::new("");

        let merged = merge_path_entries(inherited, probed);

        assert_eq!(merged.to_str(), Some("/usr/bin:/bin"));
    }
}

#[cfg(test)]
mod resolve_login_shell_path_tests {
    use super::resolve_login_shell_path;
    use std::ffi::OsStr;

    #[test]
    fn returns_none_immediately_when_the_inherited_path_already_looks_rich() {
        // Short-circuits on the richness check before touching `$SHELL` or stdin, so this is
        // deterministic regardless of the environment `cargo test` itself runs under.
        let rich = OsStr::new("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(resolve_login_shell_path(rich), None);
    }

    /// Not run by default (`cargo test` does not cover this by design - it depends on the real
    /// developer machine's `$SHELL` and rc files, not something CI can assert on). A manual
    /// sanity check for issue #447's actual bug: run with
    /// `cargo test -p pty-core --lib -- --ignored resolves_a_real_homebrew_style_path`,
    /// simulating a minimal launchd-style inherited PATH and confirming the real login shell's
    /// rc files really do add a Homebrew-style prefix on this machine.
    #[test]
    #[ignore]
    fn resolves_a_real_homebrew_style_path_from_the_actual_login_shell() {
        let minimal = OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin");
        let resolved = resolve_login_shell_path(minimal)
            .expect("resolving against the real $SHELL on this machine should succeed");
        let text = resolved.to_string_lossy();
        assert!(
            text.contains("/homebrew/") || text.contains("/usr/local/bin"),
            "expected the real login shell's PATH to include a Homebrew-style prefix, got: \
             {text:?}"
        );
    }
}

#[cfg(test)]
mod run_login_shell_probe_tests {
    use super::run_login_shell_probe;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn captures_stdout_of_a_quick_real_command() {
        let output = run_login_shell_probe(
            Path::new("/bin/sh"),
            &["-c", "echo probe-marker-pty-core"],
            Duration::from_secs(5),
        )
        .expect("running a real, quick `sh -c echo` should produce output");

        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("probe-marker-pty-core"),
            "expected the echoed marker in captured stdout, got: {text:?}"
        );
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_the_timeout_instead_of_blocking_forever() {
        // The bound is deliberately far above the timeout rather than a hair over it: what this
        // asserts is that the probe gives up on its own budget instead of running the command
        // out, and a bound tight enough to also measure *which* internal wait ended it would be
        // measuring the CI runner's scheduling latency.
        let timeout = Duration::from_millis(200);
        let started = Instant::now();

        let _ = run_login_shell_probe(Path::new("/bin/sh"), &["-c", "sleep 30"], timeout);

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "probe of a hanging command took {elapsed:?}, expected it to be killed at \
             roughly the {timeout:?} timeout instead of running to completion"
        );
    }

    #[test]
    fn a_backgrounded_grandchild_holding_the_pipe_open_does_not_hang_startup() {
        // The shell exits immediately but leaves a `sleep` holding the inherited stdout write
        // end, so `read_to_end` never sees EOF. Without a bounded wait on the reader channel
        // this returns only when the grandchild dies - 30s here, unbounded in the real rc-file
        // case this guards.
        let timeout = Duration::from_millis(200);
        let started = Instant::now();

        let _ = run_login_shell_probe(
            Path::new("/bin/sh"),
            &["-c", "sleep 30 & echo /usr/bin:/bin"],
            timeout,
        );

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "probe took {elapsed:?} with a grandchild holding the stdout pipe open, expected \
             it to give up near the {timeout:?} timeout rather than wait out the grandchild"
        );
    }

    #[test]
    fn a_nonexistent_shell_returns_none_rather_than_panicking() {
        assert_eq!(
            run_login_shell_probe(
                Path::new("/definitely/not/a/real/shell-xyz"),
                &["-c", "echo hi"],
                Duration::from_secs(1),
            ),
            None
        );
    }
}
