//! Pure, GPUI-free, network-free data model for the update-available feature (GitHub issue
//! #87): the real version-comparison decision plus the state machine `crate::updater::render`
//! draws and `crate::updater::flow` drives. No `gpui::Window`/`Context`, no network call, so
//! this stays directly `#[test]`-able - the same "pure half, GPUI half" split `crate::status_bar`
//! (`process_stats`/`render`) and `crate::worktree_history` (`flow`'s own pure
//! `branch_display_for`) already establish.

use std::time::Duration;

/// How often the background loop (`crate::updater::flow::AdeApp::start_update_check_loop`)
/// re-checks GitHub for a newer release, after the real startup check that loop also performs.
/// GitHub's unauthenticated REST API allows 60 requests/hour per source IP - at one request per
/// tick, 6h is nowhere near that budget even with the palette's manual "Check for Updates"
/// command added on top (a user would have to invoke it dozens of times an hour to matter).
pub(crate) const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Whether this build compiles the update check in at all.
pub(crate) const UPDATE_CHECK_ENABLED: bool = !cfg!(test);

/// Runtime kill switch, for anything linking `app` across a crate boundary that `cfg(test)` can
/// not reach.
pub(crate) const DISABLE_UPDATE_CHECK_ENV: &str = "JERRY_DISABLE_UPDATE_CHECK";

/// Whether a real GitHub release check may happen in this process, right now.
pub(crate) fn update_check_enabled() -> bool {
    update_check_enabled_from(
        UPDATE_CHECK_ENABLED,
        std::env::var_os(DISABLE_UPDATE_CHECK_ENV),
    )
}

/// The pure half of [`update_check_enabled`], split out because `std::env::set_var` is unsound to
/// race in a threaded test binary.
pub(crate) fn update_check_enabled_from(
    compiled_in: bool,
    disable_env: Option<std::ffi::OsString>,
) -> bool {
    compiled_in && disable_env.is_none()
}

/// A real GitHub release this app could update to - built from `self_update::update::Release`
/// (see `crate::updater::flow`'s own docs for exactly how, since that struct has no `tag_name`/
/// `html_url` field of its own to read directly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReleaseInfo {
    /// The real git tag, e.g. `"v0.2.0"` - what `release.yml` actually tags
    /// (`on: push: tags: "v*"`), and what `self_update`'s `target_version_tag` needs to fetch
    /// this exact release again for the download step.
    pub(crate) tag: String,
    /// The tag with its leading `v` stripped, e.g. `"0.2.0"` - already exactly this shape as
    /// returned by `self_update::update::Release::version` (see `crate::updater::flow`'s docs),
    /// not a second, independent strip.
    pub(crate) version: String,
    /// A real, constructed (not API-provided - `self_update::update::Release` carries no HTML
    /// URL of its own) `https://github.com/<owner>/<repo>/releases/tag/<tag>` link, for a
    /// future "view release notes" affordance. Not yet rendered anywhere (MVP scope, per the
    /// issue's own status-bar-chip-only ask) - kept on this type now rather than bolted on
    /// later, since it's free to compute alongside `tag`/`version`.
    pub(crate) html_url: String,
}

/// The update feature's whole state machine - one field on `AdeApp`
/// (`crate::root::AdeApp::update_state`), driven entirely by `crate::updater::flow`'s real
/// background checks/downloads, drawn entirely by `crate::updater::render`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum UpdateState {
    Idle,
    UpdateAvailable(ReleaseInfo),
    Downloading {
        release: ReleaseInfo,
        /// `self_update`'s own progress reporting is terminal/`indicatif`-oriented, not
        /// GPUI-friendly (see `crate::updater::flow`'s docs on `show_output(false)`/
        /// `no_confirm(true)`) - this stays `None` for the real MVP download (indeterminate
        /// "Updating…" text is enough to satisfy "loading feedback"), but is a real `Option`,
        /// not a bare unit, so a future revision can wire real percentage progress through
        /// without another `UpdateState` shape change.
        progress: Option<f32>,
    },
    ReadyToRestart {
        release: ReleaseInfo,
    },
    /// Only ever reached from a *download* failure (post-click) - never from a background
    /// check failure. See this enum's own docs and `crate::updater::flow`'s module docs.
    Failed {
        release: ReleaseInfo,
        error: String,
    },
}

/// Decides whether `remote_tag` (a real GitHub release tag, e.g. `"v0.2.0"` - the `v` prefix is
/// optional here, stripped either way, since this is also exercised directly against
/// already-stripped input in tests below) is a genuinely newer, real, actionable update over
/// `current` (this build's own `env!("CARGO_PKG_VERSION")`, never `v`-prefixed).
pub(crate) fn is_remote_version_newer(current: &str, remote_tag: &str) -> bool {
    let remote_version = remote_tag.trim_start_matches(['v', 'V']);
    if remote_version.contains('-') {
        // A real pre-release version - see this function's own docs for why this is checked
        // explicitly rather than left to semver precedence alone.
        return false;
    }
    match self_update::version::bump_is_greater(current, remote_version) {
        Ok(is_greater) => is_greater,
        Err(err) => {
            log::warn!(
                "update check: could not compare current version {current:?} against remote \
                 {remote_tag:?}, treating as no update available: {err}"
            );
            false
        }
    }
}

#[cfg(test)]
mod update_check_gate_tests {
    use crate::updater::state::{update_check_enabled, update_check_enabled_from};
    use std::ffi::OsString;

    #[test]
    fn a_release_build_with_no_override_checks_for_updates() {
        assert!(update_check_enabled_from(true, None));
    }

    #[test]
    fn a_test_build_never_checks_for_updates() {
        assert!(!update_check_enabled_from(false, None));
    }

    #[test]
    fn the_environment_override_wins_over_a_release_build() {
        assert!(!update_check_enabled_from(true, Some(OsString::from("1"))));
        // The variable's presence is the whole signal, so an empty value still disables.
        assert!(!update_check_enabled_from(true, Some(OsString::from(""))));
    }

    #[test]
    fn this_very_test_binary_may_not_check_for_updates() {
        assert!(
            !update_check_enabled(),
            "the compiled-in gate must hold for the app crate's own test targets"
        );
    }
}

#[cfg(test)]
mod version_comparison_tests {
    use super::*;

    #[test]
    fn equal_versions_are_not_an_update() {
        assert!(!is_remote_version_newer("0.1.0", "v0.1.0"));
    }

    #[test]
    fn a_real_newer_tag_is_an_update() {
        assert!(is_remote_version_newer("0.1.0", "v0.2.0"));
    }

    #[test]
    fn an_older_tag_is_not_an_update() {
        assert!(!is_remote_version_newer("0.1.0", "v0.0.9"));
    }

    #[test]
    fn an_unparseable_tag_is_gracefully_not_an_update() {
        assert!(!is_remote_version_newer("0.1.0", "garbage"));
    }

    #[test]
    fn a_pre_release_tag_is_never_an_update() {
        assert!(!is_remote_version_newer("0.1.0", "v0.1.0-beta.1"));
        // The stronger case this function's docs call out: a pre-release of a real *future*
        // version would out-rank `current` under plain semver precedence alone, but must still
        // never count as an update - this repo has no pre-release-tag convention to offer one
        // through.
        assert!(!is_remote_version_newer("0.1.0", "v0.2.0-beta.1"));
    }
}
