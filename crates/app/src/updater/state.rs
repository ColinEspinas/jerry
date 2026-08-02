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
///
/// `Idle` deliberately covers three different real situations - "never checked yet", "checked
/// and genuinely up to date", and "the most recent *check* failed (offline, DNS, GitHub
/// rate-limit, bad JSON, TLS)" - because all three render identically (nothing shown) and none
/// of them should ever interrupt normal use. Only a *download* failure (after the user has
/// already clicked "Update available") is a distinct, shown `Failed` state - see
/// `crate::updater::flow`'s module docs for the full "never intrusive on a check failure" rule
/// this distinction exists to enforce.
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
///
/// Pure and deterministic - no GPUI, no network - delegates the actual semver comparison to
/// `self_update::version::bump_is_greater` rather than adding a direct `semver` dependency on
/// top of the one `self_update` already pulls in transitively.
///
/// Three deliberate real-world safety nets beyond a plain "is it greater" check:
/// - Malformed/unparseable version strings (`bump_is_greater` returning `Err` - not real
///   semver, e.g. a hand-pushed non-release tag or a future tagging-convention change this app
///   doesn't understand yet) are treated as "no update available", not "newer" - a `log::warn!`
///   only, never a false positive that would offer to install something that isn't a real,
///   parseable release.
/// - A pre-release remote version (a `-`-suffixed semver, e.g. `"0.2.0-beta.1"`) never counts
///   as an update, even if semver ordering alone would call it newer than `current` (e.g.
///   `"0.2.0-beta.1"` really is `>` `"0.1.0"` under plain semver precedence rules) - this repo
///   has no pre-release-tag convention today (`release.yml` only ever tags real, final `vX.Y.Z`
///   releases), so treating one as "newer" would offer an update to a tag no real release
///   process here produces yet.
/// - Equal-or-older is "no update available", the ordinary case `bump_is_greater` itself
///   already handles correctly.
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
