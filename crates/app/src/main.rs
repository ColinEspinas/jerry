//! Binary entry point: resolves the target repository path from an optional CLI argument.

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
