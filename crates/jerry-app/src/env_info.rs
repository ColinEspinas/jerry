//! Real host-environment detection: is this process running inside WSL, and if not, what CPU
//! architecture is it on. Used by the terminal footer, the status bar chip, and the Settings
//! "Default environment" row (the latter two land in Revision R6) - kept as one small,
//! GPUI-free module so all three read the same answer.

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
