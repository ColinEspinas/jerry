//! Binary entry point: resolves the target repository path from an optional CLI argument.

// Matches Zed's own `crates/zed/src/main.rs:2` verbatim: on a release Windows build this drops
// the console subsystem, so no console window flashes up behind the GPUI window. The known
// consequence is that `env_logger` below then writes to a stderr that no longer exists - no
// release-Windows logs, harmless but real; see the plan for issue #447 for the follow-up.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Before anything can spawn: children join the job at creation, so a child spawned earlier
    // than this would be exactly the orphan issue #482 exists to prevent.
    #[cfg(windows)]
    app::job_object::adopt_this_process();

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    apply_display_scale_override();

    #[cfg(unix)]
    apply_login_shell_path();

    let repo_path = env::args_os().nth(1).map(PathBuf::from);

    app::run(repo_path);
    ExitCode::SUCCESS
}

/// GitHub issue #216 ("Scaling issues on Linux"): hands GPUI's X11 client the user's forced scale
/// factor, if they configured one on the Appearance page
/// (`app::settings::store::AppearanceSettings::display_scale_override`).
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn apply_display_scale_override() {
    let settings = app::settings::store::Settings::load_or_init();
    let existing = env::var(app::GPUI_X11_SCALE_FACTOR_ENV).ok();
    let Some(value) = app::x11_scale_factor_env_value(&settings, existing.as_deref()) else {
        return;
    };

    log::info!(
        "forcing {}={value} from appearance.display_scale_override (X11 sessions only; \
         restart-scoped)",
        app::GPUI_X11_SCALE_FACTOR_ENV
    );
    // SAFETY: `std::env::set_var` is `unsafe` as of this workspace's edition because the process
    // environment is global mutable state that a concurrent `getenv` in another thread can read
    // (or a concurrent `setenv` can reallocate under). Neither hazard exists here: this runs at
    // the very top of `main`, before `app::run` starts GPUI and before this process has spawned
    // any thread of its own - so there is no other thread in existence to race. Every other place
    // that needed a custom environment (see `pty_core::resolve_in_path_var` and
    // `lsp_core::client`'s injected resolvers) passes the value as an argument specifically to
    // avoid mutating the real one; `apply_login_shell_path` below is this workspace's only other
    // `set_var` call, for the same "no other thread yet" reason, with its own justification for
    // *why* it mutates the real var rather than threading a value through.
    // The one standing exception to CLAUDE.md's "no unsafe" rule - see this function's own
    // SAFETY comment above for why it's sound here.
    #[allow(unsafe_code)]
    unsafe {
        env::set_var(app::GPUI_X11_SCALE_FACTOR_ENV, value)
    };
}

/// GitHub issue #447: a Finder-launched `.app` (and a Linux `.desktop` launch) inherits
/// launchd's/the session manager's minimal PATH, not the user's shell PATH - so `claude`,
/// `codex`, `cursor` and friends read as "not installed" even though a terminal finds them fine.
/// [`pty_core::resolve_login_shell_path`] is a no-op unless the inherited PATH actually looks
/// like that minimal case, so this is safe to call unconditionally on every unix launch.
#[cfg(unix)]
fn apply_login_shell_path() {
    let inherited = env::var_os("PATH").unwrap_or_default();
    let Some(resolved) = pty_core::resolve_login_shell_path(&inherited) else {
        return;
    };

    log::info!(
        "resolved login-shell PATH for a GUI launch (issue #447): {:?} -> {resolved:?}",
        inherited
    );
    // SAFETY: same argument as `apply_display_scale_override` above - this runs at the very top
    // of `main`, before `app::run` starts GPUI and before this process has spawned any thread of
    // its own, so there is no other thread in existence to race a concurrent `getenv`/`setenv`.
    //
    // Setting the real process PATH (rather than threading the resolved value through
    // `pty_core::resolve_on_path`'s callers) is deliberate, not an oversight: every agent PTY
    // `pty_core::spawn` launches inherits the process environment, so mutating it here is what
    // makes terminal panes get the corrected PATH for free, not just the agent-detection UI.
    #[allow(unsafe_code)]
    unsafe {
        env::set_var("PATH", resolved)
    };
}
