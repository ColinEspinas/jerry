//! Real host-environment detection: is this process really running inside WSL, and (if not)
//! what real CPU architecture is it on. `design_handoff_jerry_ade/revision/CHANGELOG.md`'s
//! 2026-07-29 entry, change 8 ("Environment (WSL) chip"), names three real call sites that all
//! need the exact same answer - the terminal footer band (this phase, Revision R4b), and the
//! status bar chip plus the Settings `Default environment` row (both tracked as Revision R6) -
//! so this lives in one small, GPUI-free, reusable module from the start rather than being
//! introduced inline in the terminal footer and left for R6 to extract out later.
//!
//! ## The real detection signal, and why
//!
//! [`is_wsl`] checks `WSL_DISTRO_NAME` - the real environment variable WSL itself sets, on
//! every distro, for exactly this purpose (Microsoft's own WSL interop tooling, and the
//! standard signal countless real-world scripts already key off) - as its *primary* signal.
//! This was chosen over parsing `/proc/version` for a `"microsoft"`/`"WSL"` marker for two real
//! reasons: it's a direct, purpose-built signal rather than a string-match against a kernel
//! version banner that happens to (usually) contain similar text, and it needs no blocking file
//! read at all - a plain `std::env::var_os` lookup, safe to call from anywhere, including a
//! render path, with no I/O-on-the-render-thread concern the way a `/proc/version` read would
//! raise (this project's own established "no blocking I/O on the GPUI foreground thread" rule -
//! see `crate::root`'s module docs for the pattern this avoids needing to think about at all).
//!
//! The env var alone isn't always enough, though: it's set by WSL's own interop layer at shell
//! startup, so a process launched a different way - e.g. under systemd-managed launch, which is
//! common for a GUI app started from a desktop entry rather than a login shell - may never
//! inherit it, even on a real, current WSL2 kernel. [`is_wsl`] falls back to checking whether
//! `/run/WSL` exists (a marker file WSL2 itself reliably creates, independent of any process's
//! own environment) when the env var is absent, so a real WSL2 process launched either way is
//! still detected correctly.
//!
//! [`is_wsl`] itself is a thin, un-testable-by-construction wrapper (it reads the real,
//! process-wide environment and filesystem) around [`is_wsl_from`], the actual pure decision -
//! injected with two plain `bool`s (one per real signal) rather than a real env/filesystem
//! lookup, so every combination is directly, deterministically unit-testable without depending
//! on whatever environment the test runner itself happens to execute in (this sandbox's own dev
//! machine is genuinely WSL2, so a test that only ever read the real signals could never
//! exercise the "neither present" branch at all).
//!
//! ## Caching
//!
//! Both real signals ([`is_wsl`]'s env-var-or-`/run/WSL` check, and [`wsl_distro_name`]'s own
//! env var read) are constant for the lifetime of this process - WSL-ness doesn't change while
//! this app is running. [`is_wsl`]/[`wsl_distro_name`] each cache their real result behind a
//! `std::sync::OnceLock`, computed once on first use rather than re-derived (a `std::env::var`
//! lookup plus a `String` allocation, for [`wsl_distro_name`]) on every single call - both are
//! read from a real render path (`crate::root::widgets::render_env_chip`,
//! `crate::root::work_surface_render::render_pty_header`), so "every render" was the real,
//! non-theoretical call frequency this was pointless repeated work against.

use std::path::Path;
use std::sync::OnceLock;

/// The real environment variable WSL sets - see the module docs for why this, not
/// `/proc/version`, is the primary signal checked.
const WSL_DISTRO_ENV_VAR: &str = "WSL_DISTRO_NAME";

/// The real fallback signal checked when [`WSL_DISTRO_ENV_VAR`] is absent - see the module
/// docs for why the env var alone isn't always sufficient.
const WSL_RUN_MARKER_PATH: &str = "/run/WSL";

/// `true` when this process is really running inside WSL right now. Cached for the life of the
/// process after the first call - see the module docs' "Caching" section.
pub fn is_wsl() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        is_wsl_from(
            std::env::var_os(WSL_DISTRO_ENV_VAR).is_some(),
            Path::new(WSL_RUN_MARKER_PATH).exists(),
        )
    })
}

/// The pure decision [`is_wsl`] wraps - see the module docs for why this split exists, and why
/// it takes both real signals rather than just the env var.
fn is_wsl_from(has_wsl_distro_env_var: bool, has_run_wsl_marker: bool) -> bool {
    has_wsl_distro_env_var || has_run_wsl_marker
}

/// The real distro name WSL reports (`WSL_DISTRO_NAME`'s own value, e.g. `"Ubuntu"`), if this
/// process is genuinely running inside WSL - `None` both when the env var itself is absent
/// (including the case where [`is_wsl`] is `true` only via the `/run/WSL` fallback signal, which
/// carries no distro name of its own) and, defensively, if the variable is somehow set but
/// empty (a real if unlikely misconfiguration - treated the same as "not WSL" rather than
/// rendering an empty chip label). Cached for the life of the process after the first call -
/// see the module docs' "Caching" section; returns a real `&'static str` rather than a fresh
/// owned `String` per call precisely so that caching actually avoids the repeated allocation,
/// not just the repeated `std::env::var` syscall.
pub fn wsl_distro_name() -> Option<&'static str> {
    static CACHED: OnceLock<Option<String>> = OnceLock::new();
    CACHED
        .get_or_init(|| {
            std::env::var(WSL_DISTRO_ENV_VAR)
                .ok()
                .filter(|name| !name.is_empty())
        })
        .as_deref()
}

/// The real CPU architecture string (`"x86_64"`, `"aarch64"`, ...) - `std::env::consts::ARCH`,
/// real Rust `std`, backing the environment chip's `local · <arch>` fallback when [`is_wsl`] is
/// `false`. A thin named wrapper (rather than every call site reaching for
/// `std::env::consts::ARCH` directly) purely so every real caller reads through this one
/// module, matching [`is_wsl`]/[`wsl_distro_name`].
pub fn local_arch() -> &'static str {
    std::env::consts::ARCH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_var_alone_is_detected_as_wsl() {
        assert!(is_wsl_from(true, false));
    }

    /// The real fallback this module's own docs describe: a real WSL2 process that never
    /// inherited `WSL_DISTRO_NAME` (e.g. a systemd-managed GUI launch) must still be detected
    /// as WSL via the second, independent `/run/WSL` signal alone.
    #[test]
    fn the_run_wsl_marker_alone_is_also_detected_as_wsl() {
        assert!(is_wsl_from(false, true));
    }

    #[test]
    fn both_signals_present_is_still_detected_as_wsl() {
        assert!(is_wsl_from(true, true));
    }

    #[test]
    fn neither_signal_present_is_detected_as_not_wsl() {
        assert!(!is_wsl_from(false, false));
    }

    #[test]
    fn local_arch_returns_the_real_std_env_consts_arch() {
        assert_eq!(local_arch(), std::env::consts::ARCH);
    }
}
