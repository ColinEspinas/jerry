//! The real `self_update`-backed background work behind this feature (GitHub issue #87): the
//! periodic/manual/startup update *check*, the click-to-download *update*, and the new
//! relaunch-and-exit mechanism, all as `impl AdeApp` background-task methods - the exact same
//! "spawn on the background executor, only touch `self` again inside `this.update(cx, ..)` once
//! it resolves" shape `crate::worktree_history::flow::AdeApp::keep_all_changes` already
//! establishes for a real, blocking, non-GPUI call.
//!
//! ## API surface actually used, verified against the real, pinned `self_update = "0.44.0"`
//!
//! Verified directly against that version's real published source
//! (`docs.rs/crate/self_update/0.44.0/source/src/{update,backends/github}.rs`), not assumed
//! from the crate's own top-level README (which, as of this writing, describes a newer,
//! not-yet-released `1.0.0-rc` API shape with different method/module names) - the exact
//! mismatch this crate's own `Cargo.toml` comment on the `self_update` dependency warns about:
//!
//! - `self_update::update::Release` (what both `ReleaseUpdate::get_latest_release` and
//!   `backends::github::ReleaseList::fetch` return) has fields `name`, `version`, `date`,
//!   `body`, `assets` - **no** `tag_name`/`html_url` field. Real, read source:
//!   `Release::from_release` (the GitHub-JSON-to-`Release` conversion) sets
//!   `version: tag.trim_start_matches('v')` - so `release.version` is already the real tag with
//!   its leading `v` stripped, and the real tag itself is recovered as `format!("v{}",
//!   release.version)` (safe: this repo's own `release.yml` only ever tags `vX.Y.Z`, never a
//!   bare `X.Y.Z`). `html_url` is genuinely not provided by this struct at all - `release_url`
//!   below constructs the real, standard GitHub URL shape by hand instead of reading one back.
//! - `ReleaseUpdate::get_latest_release(&self)` really does call `GET
//!   /repos/<owner>/<repo>/releases/latest` (confirmed directly in `backends/github.rs`'s
//!   `impl ReleaseUpdate for Update`) - GitHub's own documented behavior for that endpoint
//!   already excludes drafts/prereleases, matching `release.yml`'s draft-on-`workflow_dispatch`
//!   behavior with no extra filtering needed on this crate's side.
//! - `Release::asset_for(target, identifier)` (what `Update::update()` uses internally to pick
//!   an asset) matches via `asset.name.contains(target)` - confirmed directly in `update.rs` -
//!   so `.target(std::env::consts::OS)` (`"linux"`/`"macos"`/`"windows"`, never a Rust target
//!   triple) is a real, correct match against `release.yml`'s actual asset names
//!   (`jerry-linux.tar.gz`/`jerry-macos.tar.gz`/`jerry-windows.zip`) - `self_update::get_target()`
//!   (a Rust target triple like `x86_64-unknown-linux-gnu`) would never match any of them.
//! - `UpdateBuilder::build()` returns `Result<Box<dyn ReleaseUpdate>>`, not a bare `Update` -
//!   [`build_update`] returns that trait object directly, used for both the check
//!   ([`check_for_update_blocking`], via `get_latest_release`) and the download
//!   ([`run_self_update`], via `update()`) - one real, shared builder call, not two independent
//!   configurations that could drift apart.
//!
//! ## Never intrusive on a *check* failure
//!
//! See this module's parent (`crate::updater`) docs for the full rule. The one real enforcement
//! point is [`AdeApp::check_for_update`]'s completion handler: an `Err` only `log::warn!`s -
//! [`state::UpdateState`] is left completely untouched, and `cx.notify()` is never called. A
//! *successful* check result also deliberately never clobbers an in-flight/just-finished
//! *download* (`Downloading`/`ReadyToRestart`) - a periodic re-check firing while the user is
//! mid-download (or has already finished and is looking at "Restart to update") must not stomp
//! that with a fresh "Update available" for the same release; this app's own `current_version`
//! doesn't change until the actual relaunch, so a background re-check genuinely would otherwise
//! see the exact same release as "still available" and silently revert real, further-along
//! progress the user is already looking at.
//!
//! ## What has and hasn't been really exercised
//!
//! `ColinEspinas/jerry` has a real, published `v0.0.1` release (assets: `jerry-linux.tar.gz`,
//! `jerry-macos.tar.gz`, `jerry-windows.zip`, exactly matching `release.yml`'s naming), and
//! [`check_for_update_blocking`]/a `self_update` download configured identically to
//! [`build_update`] (just with `bin_install_path` redirected to a scratch directory instead of
//! `current_exe()`, to stay safe to run repeatedly from a dev shell) were manually run against it
//! for real while developing this feature - confirming a real `Ok(Some(release))` detection of
//! `v0.0.1`, a real "no update" result against a locally-newer version, and a real
//! download+match+extract producing a genuine, executable file on disk. That verification is
//! **deliberately not checked in as an automated test**: it depends on GitHub's live,
//! unauthenticated API, which enforces a 60-requests/hour-per-IP limit - a real hazard for a
//! test that would otherwise run routinely (every local `cargo test`, every CI run, and any
//! shared/CI-adjacent network sharing that IP with other traffic can exhaust that budget, which
//! is exactly what was observed while developing this: a real `403`/`x-ratelimit-remaining: 0`
//! from GitHub mid-verification). A flaky, network-dependent, rate-limit-prone test would be a
//! worse trade than the plain manual verification already performed. [`run_self_update`]'s
//! actual self-replace-of-the-running-executable branch and the following
//! [`AdeApp::relaunch_and_exit`] process-spawn-and-`cx.quit()` cycle remain unexercised even
//! manually (reasoned about from the real, pinned `self_update`/`gpui` source only - see
//! [`run_self_update`]'s and [`AdeApp::relaunch_and_exit`]'s own docs), as does the status bar
//! chip's actual GPUI rendering/click behavior (no display server available while developing
//! this) and Windows/macOS-specific behavior (`.bin_name` without `.exe`,
//! `Compress-Archive`-packaged zip extraction - developed on Linux only). Matches this
//! codebase's own established convention for this kind of gap
//! (`crate::root::AdeApp::open_install_url`'s docs).

use super::*;
use std::ffi::OsString;
use std::path::Path;

const REPO_OWNER: &str = "ColinEspinas";
const REPO_NAME: &str = "jerry";
const BIN_NAME: &str = "jerry";

/// One real, shared `self_update` builder configuration for both the check
/// ([`check_for_update_blocking`]) and the download ([`run_self_update`]) paths - see this
/// module's own docs for why each field is set the way it is. `target_version_tag` is only set
/// for the download path (`Some(tag)`); the check path leaves it unset so
/// `ReleaseUpdate::get_latest_release` (not `get_release_version`) is what actually gets called.
fn build_update(
    current_version: &str,
    target_version_tag: Option<&str>,
) -> self_update::errors::Result<Box<dyn self_update::update::ReleaseUpdate>> {
    let mut builder = self_update::backends::github::Update::configure();
    builder
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name(BIN_NAME)
        // Real asset-name-`contains` match, not a Rust target triple - see this module's docs.
        .target(std::env::consts::OS)
        .current_version(current_version)
        // This is a GUI process, not a terminal self-updater CLI (what `self_update` was
        // designed for) - `no_confirm(true)` skips its stdin `y/N` prompt (which would
        // otherwise hang forever with no terminal attached) and `show_output(false)` skips its
        // `indicatif` terminal progress bar. Reasoned from `update.rs`'s real, read source
        // (`print_flush`/`println` calls are all gated behind `show_output`, and the
        // confirmation prompt behind `no_confirm`) - not smoke-tested end to end as a real GUI
        // process performing a real download yet, see this module's own top-level docs.
        .no_confirm(true)
        .show_output(false);
    if let Some(tag) = target_version_tag {
        builder.target_version_tag(tag);
    }
    builder.build()
}

/// The real, standard GitHub release URL for `tag` - constructed by hand, not read from
/// `self_update::update::Release` (which has no such field - see this module's own docs).
fn release_url(tag: &str) -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/tag/{tag}")
}

fn release_info_from(release: self_update::update::Release) -> state::ReleaseInfo {
    let tag = format!("v{}", release.version);
    state::ReleaseInfo {
        html_url: release_url(&tag),
        version: release.version,
        tag,
    }
}

/// The real, blocking (background-executor-only, never called from the foreground thread)
/// GitHub API call behind [`AdeApp::check_for_update`] - a real `GET
/// /repos/ColinEspinas/jerry/releases/latest`, then [`state::is_remote_version_newer`] against
/// this build's own `current_version`. `Ok(None)` means "reached GitHub fine, genuinely up to
/// date, or the tag was malformed" (see that function's own docs); `Err` means "the check itself
/// failed" (offline, rate-limited, ...) - the two cases [`AdeApp::check_for_update`]'s completion
/// handler treats very differently, per this module's own "never intrusive on a check failure"
/// docs.
fn check_for_update_blocking(current_version: &str) -> anyhow::Result<Option<state::ReleaseInfo>> {
    let updater = build_update(current_version, None)
        .map_err(|err| anyhow::anyhow!("could not configure the update check: {err}"))?;
    let release = updater
        .get_latest_release()
        .map_err(|err| anyhow::anyhow!("could not reach GitHub's releases API: {err}"))?;
    let tag = format!("v{}", release.version);
    if !state::is_remote_version_newer(current_version, &tag) {
        return Ok(None);
    }
    Ok(Some(release_info_from(release)))
}

/// The real, blocking download-extract-self-replace call behind
/// [`AdeApp::start_update_download`]: `self_update::backends::github::Update::update()`, real
/// per that crate's own source (see this module's top-level docs). Downloads `release`'s
/// matching archive asset, extracts it (`archive-tar`/`archive-zip` +
/// `compression-flate2`/`compression-zip-deflate`, matching this crate's own `Cargo.toml`
/// feature selection), and atomically replaces the currently-running executable via
/// `self-replace` - all synchronous, hence only ever called from the background executor (see
/// [`AdeApp::start_update_download`]).
///
/// **Partially, manually verified - not via a committed test.** The download+match+extract half
/// of this call was manually run for real against the real `v0.0.1` release while developing
/// this feature (with `bin_install_path` redirected to a scratch directory, not left at
/// `current_exe()`, to stay safe to run repeatedly) and genuinely produced a runnable binary; see
/// this module's own top-level docs for why that verification wasn't checked in as an automated
/// test (GitHub's unauthenticated rate limit makes a routinely-run real-network test a bad
/// trade). The actual self-replace-of-the-running-executable branch this function takes in
/// production (`bin_install_path == current_exe()` → `self_replace::self_replace`, never
/// exercised, vs. the `Move::from_source(..).to_dest(..)` branch the manual verification above
/// took) remains reasoned about only, from the real, pinned `self_update` source. Matches this
/// codebase's established "explicit about what was smoke-tested vs. only reasoned about"
/// convention (`crate::root::AdeApp::open_install_url`'s docs).
fn run_self_update(
    current_version: &str,
    release: &state::ReleaseInfo,
) -> self_update::errors::Result<self_update::Status> {
    build_update(current_version, Some(&release.tag))?.update()
}

/// Real, pure command construction for [`AdeApp::relaunch_and_exit`] - split out for direct unit
/// testing with no real process spawn, matching `crate::settings::widgets::open_command_for`'s
/// own "pure decision function + thin real-execution wrapper" precedent (that function's own
/// docs name this exact convention).
fn build_relaunch_command(exe: &Path, args: &[OsString]) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command.args(args);
    command
}

impl AdeApp {
    /// The real, background GitHub-releases-backed update check - the palette's "Check for
    /// Updates" command's real target ([`crate::palette::state::PaletteCommand::CheckForUpdates`]),
    /// and what [`Self::start_update_check_loop`] calls both immediately at startup and on every
    /// subsequent tick. No-op while a check is already in flight (mirrors
    /// [`Self::worktree_history_op_in_flight`]'s own single-flight-per-feature discipline), so a
    /// manual palette click during the periodic loop's own in-flight check can never spawn a
    /// second, racing one.
    ///
    /// See this module's own docs for exactly how a `Result::Err` (a genuine check failure) and
    /// a real `Ok` result are each handled - the one place in this whole feature the "never
    /// intrusive on a check failure" rule is actually enforced.
    ///
    /// Deliberately `.detach()`es its own spawned task rather than storing it in
    /// [`Self::_update_check_task`] (unlike almost every other background call in this crate):
    /// [`Self::start_update_check_loop`]'s own persistent loop calls this method from *within*
    /// the very task already sitting in that field, on every tick - if this method also
    /// reassigned `_update_check_task`, that call would drop (and, per this crate's own
    /// documented `Task`-drop-cancels semantics, cancel) the outer loop's own task out from
    /// under itself, permanently ending the periodic loop after its very first tick. A real,
    /// live-reasoned hazard, not a hypothetical one - `_update_check_task` is reserved solely
    /// for that outer loop; [`Self::update_check_in_flight`] alone (not a stored `Task`) is what
    /// guards this method against a second, overlapping one-shot check, matching
    /// `Self::spawn_open_command`'s own established "detach a background op nothing needs to
    /// later cancel" precedent.
    pub(crate) fn check_for_update(&mut self, cx: &mut Context<Self>) {
        if self.update_check_in_flight {
            return;
        }
        self.update_check_in_flight = true;
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { check_for_update_blocking(&current_version) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.update_check_in_flight = false;
                match result {
                    Ok(fresh) => {
                        // A background/manual check's own fresh, real result must never
                        // overwrite an in-flight or just-finished *download* - see this
                        // module's own docs for why.
                        if !matches!(
                            this.update_state,
                            state::UpdateState::Downloading { .. }
                                | state::UpdateState::ReadyToRestart { .. }
                        ) {
                            this.update_state = match fresh {
                                Some(release) => state::UpdateState::UpdateAvailable(release),
                                None => state::UpdateState::Idle,
                            };
                            cx.notify();
                        }
                    }
                    Err(err) => {
                        // Never touched, never notified - see this module's own "never
                        // intrusive on a check failure" docs.
                        log::warn!(
                            "update check failed (this is expected on flaky/offline \
                                     networks and never surfaced to the user): {err}"
                        );
                    }
                }
            });
        })
        .detach();
    }

    /// Starts the real startup-plus-periodic update-check loop - called once, from
    /// `Self::new_with_settings`, right after `Self::start_worktree_watch`. Runs
    /// [`Self::check_for_update`] immediately (the real "startup" trigger), then again every
    /// [`state::UPDATE_CHECK_INTERVAL`] for as long as the app runs, mirroring
    /// `Self::start_worktree_watch`'s own tick-loop shape.
    pub(crate) fn start_update_check_loop(&mut self, cx: &mut Context<Self>) {
        self.check_for_update(cx);
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(state::UPDATE_CHECK_INTERVAL)
                .await;
            let updated = this.update(cx, |this, cx| {
                this.check_for_update(cx);
            });
            if updated.is_err() {
                break;
            }
        });
        self._update_check_task = Some(task);
    }

    /// The status bar chip's click handler while `update_state` is genuinely
    /// [`state::UpdateState::UpdateAvailable`] - a real, blocking, background-executor-only
    /// download+extract+self-replace via [`run_self_update`]. No-op for any other
    /// `update_state` (including a second click mid-download - [`render`]'s own chip isn't even
    /// clickable then, but this guard is the real, load-bearing one).
    pub(crate) fn start_update_download(&mut self, cx: &mut Context<Self>) {
        let state::UpdateState::UpdateAvailable(release) = self.update_state.clone() else {
            return;
        };
        self.update_state = state::UpdateState::Downloading {
            release: release.clone(),
            progress: None,
        };
        cx.notify();

        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let task = cx.spawn(async move |this, cx| {
            let release_for_call = release.clone();
            let result = cx
                .background_executor()
                .spawn(async move { run_self_update(&current_version, &release_for_call) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.update_state = match result {
                    Ok(_status) => state::UpdateState::ReadyToRestart { release },
                    Err(err) => state::UpdateState::Failed {
                        release,
                        error: err.to_string(),
                    },
                };
                cx.notify();
            });
        });
        self._update_download_task = Some(task);
    }

    /// The status bar chip's click handler while `update_state` is genuinely
    /// [`state::UpdateState::ReadyToRestart`] - relaunches the just-self-replaced executable and
    /// exits this process. No-op for any other `update_state`, same discipline as
    /// [`Self::start_update_download`].
    pub(crate) fn restart_after_update(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.update_state, state::UpdateState::ReadyToRestart { .. }) {
            return;
        }
        self.relaunch_and_exit(cx);
    }

    /// Spawns a fresh copy of this same executable (now the just-self-replaced, newer binary)
    /// with the same CLI arguments this process itself was launched with, then quits this
    /// process - genuinely new to this codebase; nothing here relaunched the process before this
    /// issue (`Self::restart_lsp_clients` is an unrelated, differently-named mechanism that only
    /// respawns language-server child processes, never this app itself).
    ///
    /// `std::env::args_os().skip(1)` preserves the repo-path CLI argument `main.rs`'s own
    /// `run` reads (skipping only `args_os()`'s first element, the program path itself, which
    /// the new `std::process::Command::new(exe)` call below supplies fresh anyway) - so the
    /// relaunched process opens the same repo/window this one did, not a bare, argument-less
    /// invocation. `cx.quit()` (real, verified against `vendor/zed`'s own
    /// `gpui::App::quit`/`on_window_close_quit.rs` example - "gracefully quit the application
    /// via the platform's standard routine") is used rather than a raw `std::process::exit(0)`,
    /// so this process's own real GPUI/OS teardown still runs.
    ///
    /// Only reachable through [`Self::restart_after_update`]'s own `ReadyToRestart` guard - if
    /// `std::env::current_exe()` or the real spawn itself fails (both real, honestly-reported
    /// possibilities - e.g. the executable was moved out from under the running process), this
    /// `log::warn!`s and returns without quitting, leaving `update_state` at `ReadyToRestart` so
    /// the user can simply try the click again or restart the app by hand.
    pub(crate) fn relaunch_and_exit(&mut self, cx: &mut Context<Self>) {
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(err) => {
                log::warn!("cannot relaunch after update: current_exe() failed: {err}");
                return;
            }
        };
        let args: Vec<OsString> = std::env::args_os().skip(1).collect();
        match build_relaunch_command(&exe, &args).spawn() {
            Ok(_child) => {
                cx.quit();
            }
            Err(err) => {
                log::warn!("failed to relaunch after update: {err}");
            }
        }
    }
}

#[cfg(test)]
mod relaunch_command_tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn build_relaunch_command_uses_the_given_exe_and_preserves_every_arg() {
        let exe = Path::new("/usr/local/bin/jerry");
        let args = vec![
            OsString::from("/home/user/my-repo"),
            OsString::from("--some-flag"),
        ];

        let command = build_relaunch_command(exe, &args);

        assert_eq!(command.get_program(), OsStr::new("/usr/local/bin/jerry"));
        let collected: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(
            collected,
            vec![OsStr::new("/home/user/my-repo"), OsStr::new("--some-flag"),],
            "every real CLI arg (e.g. the repo-path argument main.rs reads) must be preserved, \
             in order"
        );
    }

    #[test]
    fn build_relaunch_command_with_no_args_passes_none_through() {
        let exe = Path::new("/usr/local/bin/jerry");
        let command = build_relaunch_command(exe, &[]);
        assert_eq!(command.get_args().count(), 0);
    }
}
