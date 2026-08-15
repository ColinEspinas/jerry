//! Binary entry point: resolves the target repository path from an optional CLI argument.
//!
//! GitHub issue #90: this deliberately no longer falls back to `env::current_dir()` - a fresh
//! launch with no CLI argument now hands `app::run` a real `None` instead of silently forcing
//! whatever directory the process happened to be started from open as a "repo". `app::run` (and,
//! beneath it, `root::AdeApp::new_with_settings`) resolves what to do with `None` itself: reopen
//! the last-remembered folder if one was persisted, or a genuinely empty window if not - see
//! those functions' own docs.
//!
//! GitHub issue #216 adds the one other thing that genuinely has to happen out here rather than
//! inside `app::run`: exporting the persisted X11 scale-factor override, which GPUI only reads
//! from the environment, once, as its X11 client starts. The decision itself lives in
//! `app::x11_scale_factor_env_value` (which is unit-tested); this file stays the thin glue it has
//! always been.

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    apply_display_scale_override();

    let repo_path = env::args_os().nth(1).map(PathBuf::from);

    app::run(repo_path);
    ExitCode::SUCCESS
}

/// GitHub issue #216 ("Scaling issues on Linux"): hands GPUI's X11 client the user's forced scale
/// factor, if they configured one on the Appearance page
/// (`app::settings::store::AppearanceSettings::display_scale_override`).
///
/// Scoped to the two targets that actually build a GPUI backend able to read this - see
/// `crates/app/Cargo.toml`, where `gpui_platform`'s `x11` feature is requested under
/// `cfg(any(target_os = "linux", target_os = "freebsd"))`. On a Wayland session the variable is
/// simply never read, which is what the settings row's own hint says.
///
/// `Settings::load_or_init` is a plain synchronous file read with no GPUI dependency, so calling
/// it here - before any `App` exists - is not a layering violation; the same load happens again
/// inside `app::run` for the window itself.
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
    // any thread of its own - so there is no other thread in existence to race. It is also the
    // only `set_var` call in this workspace; every other place that needed a custom environment
    // (see `pty_core::resolve_in_path_var` and `lsp_core::client`'s injected resolvers) passes
    // the value as an argument specifically to avoid mutating the real one.
    // The one standing exception to CLAUDE.md's "no unsafe" rule - see this function's own
    // SAFETY comment above for why it's sound here.
    #[allow(unsafe_code)]
    unsafe {
        env::set_var(app::GPUI_X11_SCALE_FACTOR_ENV, value)
    };
}
