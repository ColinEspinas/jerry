//! Real host-environment detection: is this process running inside WSL, and if not, what CPU
//! architecture is it on. Used by the terminal footer, the status bar chip, and the Settings
//! "Default environment" row (the latter two land in Revision R6) - kept as one small,
//! GPUI-free module so all three read the same answer.
//!
//! ## Detection
//!
//! [`is_wsl`] checks `WSL_DISTRO_NAME` first - the env var WSL itself sets on every distro for
//! exactly this purpose - rather than parsing `/proc/version`, since it needs no blocking file
//! read. That var is only set by WSL's interop layer at shell startup, so a process launched a
//! different way (e.g. systemd-managed) may never inherit it even on a real WSL2 kernel;
//! [`is_wsl`] falls back to checking whether `/run/WSL` exists (a marker file WSL2 itself
//! creates) when the env var is absent.
//!
//! [`is_wsl_from`] is the pure decision, injected with both signals as plain `bool`s so every
//! combination is unit-testable regardless of what environment the test runner itself runs in.
//!
//! Both signals are constant for the process lifetime, so [`is_wsl`]/[`wsl_distro_name`] cache
//! behind a `OnceLock` rather than re-deriving on every render.

use std::path::Path;
use std::sync::OnceLock;

const WSL_DISTRO_ENV_VAR: &str = "WSL_DISTRO_NAME";
const WSL_RUN_MARKER_PATH: &str = "/run/WSL";

/// `true` when this process is running inside WSL. Cached for the process lifetime.
pub fn is_wsl() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        is_wsl_from(
            std::env::var_os(WSL_DISTRO_ENV_VAR).is_some(),
            Path::new(WSL_RUN_MARKER_PATH).exists(),
        )
    })
}

fn is_wsl_from(has_wsl_distro_env_var: bool, has_run_wsl_marker: bool) -> bool {
    has_wsl_distro_env_var || has_run_wsl_marker
}

/// The distro name WSL reports (`WSL_DISTRO_NAME`, e.g. `"Ubuntu"`) - `None` if the env var is
/// absent (including when [`is_wsl`] is `true` only via the `/run/WSL` fallback, which carries
/// no distro name) or empty. Cached for the process lifetime.
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

/// The CPU architecture (`"x86_64"`, `"aarch64"`, ...) - backs the environment chip's
/// `local · <arch>` fallback when [`is_wsl`] is `false`.
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

    /// A real WSL2 process that never inherited `WSL_DISTRO_NAME` (e.g. a systemd-managed
    /// launch) must still be detected via the `/run/WSL` marker alone.
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
