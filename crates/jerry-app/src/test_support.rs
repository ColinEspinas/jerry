//! GPUI-flavoured test fixtures: opening a real `AdeApp` in a test window.
//!
//! Deliberately *not* part of `crates/test-support`, which the gpui-free core crates
//! dev-depend on (`docs/architecture/decisions.md` §1) — a Cargo feature would not be enough,
//! since workspace feature unification would pull `gpui` into their dev graph anyway. Repo,
//! git and wait fixtures with no GPUI in them belong there, not here.

use crate::root::AdeApp;
use crate::settings::state::AGENT_KINDS;
use crate::settings::store as settings_store;
use gpui::{Entity, TestAppContext, VisualTestContext};
use std::path::{Path, PathBuf};

/// A temporary directory whose [`Self::path`] is already the root `AdeApp` will resolve it to.
///
/// `AdeApp` canonicalizes every repo root it is handed ([`crate::rail::repo::canonical_repo_path`])
/// and then keys repos, worktrees, agents and open files by exact-path equality or `strip_prefix`
/// against it. On macOS `std::env::temp_dir()` is behind a `/var` -> `/private/var` symlink, so a
/// fixture that hands the app a bare `tempfile::TempDir::path()` and then builds its expected
/// values from that same uncanonicalized path compares two different strings for the same
/// directory, and every worktree-scoped lookup in the test silently misses.
pub(crate) struct TempRoot {
    /// Held for its `Drop`; the directory is removed when this value is.
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl TempRoot {
    pub(crate) fn path(&self) -> &Path {
        &self.root
    }

    pub(crate) fn to_path_buf(&self) -> PathBuf {
        self.root.clone()
    }

    /// Writes `contents` to a `self`-relative `name`, creating parent directories, and returns the
    /// absolute path.
    pub(crate) fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directories");
        }
        std::fs::write(&path, contents).expect("write fixture file");
        path
    }
}

/// An empty [`TempRoot`] — for the tests whose subject is a directory, not a repository.
pub(crate) fn temp_root() -> TempRoot {
    let dir = tempfile::tempdir().expect("tempdir");
    canonicalized(dir)
}

/// A [`TempRoot`] holding `test_support::seed_repo`'s repository: branch `main`, one commit
/// (`file.txt`), clean tree.
pub(crate) fn temp_repo() -> TempRoot {
    canonicalized(test_support::seed_repo())
}

/// A [`TempRoot`] whose repository `seed` builds against the canonicalized root, so a fixture that
/// seeds commits and one that records app state agree on a single spelling of the path.
pub(crate) fn temp_repo_with(seed: impl FnOnce(&Path)) -> TempRoot {
    let root = canonicalized(tempfile::tempdir().expect("tempdir"));
    seed(root.path());
    root
}

fn canonicalized(dir: tempfile::TempDir) -> TempRoot {
    // `dunce`, matching `crate::rail::repo::canonical_repo_path`: std's Windows spelling is the
    // verbatim `\\?\C:\...` form, which git rejects - a fixture carrying it fails every
    // `git worktree add`/`git add` in setup (GitHub issue #467).
    let root = dunce::canonicalize(dir.path()).expect("canonicalize tempdir");
    TempRoot { _dir: dir, root }
}

/// Opens an `AdeApp` in a test GPUI window against `repo_path`.
///
/// Uses `AdeApp::new_with_settings` with an in-memory `Settings::default()` and no settings
/// path, so a test never reads or writes the developer's real `settings.toml`.
pub(crate) fn open_test_app(
    cx: &mut TestAppContext,
    repo_path: PathBuf,
) -> (Entity<AdeApp>, &mut VisualTestContext) {
    open_test_app_with_settings(cx, repo_path, settings_store::Settings::default(), None)
}

/// [`open_test_app`] with an explicit settings value and, optionally, a real on-disk settings
/// path — for the tests that assert on what actually gets persisted.
pub(crate) fn open_test_app_with_settings(
    cx: &mut TestAppContext,
    repo_path: PathBuf,
    settings: settings_store::Settings,
    settings_path: Option<PathBuf>,
) -> (Entity<AdeApp>, &mut VisualTestContext) {
    // A test-process death nextest inflicts (a timed-out test's `TerminateProcess`, #440's ~60
    // known Windows failures) runs no `Drop`, so a real agent's child would otherwise outlive
    // it just as it would outlive a force-killed Jerry (GitHub issue #482's own rationale).
    // Idempotent, so calling it once per test app is safe.
    #[cfg(windows)]
    crate::job_object::adopt_this_process();

    cx.add_window_view(|window, cx| {
        // `AdeApp::new_with_settings` itself installs a real in-process host, driven by the test
        // executor; it has no socket, so `jerry` never finds a test instance.
        let mut app =
            AdeApp::new_with_settings(Some(repo_path), true, settings, settings_path, window, cx);
        // No `ui`-tier test may launch a real agent CLI (GitHub issue #530) - every kind spawns
        // this stub instead, which stays alive until its stdin closes and then exits 0, so a
        // spawned "agent" behaves like a real, live process for as long as a test needs one.
        for kind in AGENT_KINDS {
            let (program, args) = agent_stub_command();
            app.agents.override_binary(kind, program, args);
        }
        app
    })
}

/// A real, dispatched `RebaseStart` needs `jerry_core::jerry_binary::locate` to find a real,
/// executable `jerry` - called at the top of every fixture that goes on to start a real rebase,
/// so a missing binary fails right here with an actionable cause instead of as a confusing
/// downstream state assertion once the rebase never ran.
pub(crate) fn assert_real_jerry_binary_available() {
    assert!(
        jerry_core::jerry_binary::locate().is_some(),
        "no real `jerry` binary found next to this test binary - run `cargo build -p jerry-cli` \
         first"
    );
}

/// The stub every `ui`-tier test app spawns in place of a real `claude`/`codex`/`cursor-agent`
/// (GitHub issue #530) - see [`open_test_app_with_settings`]. Windows: `cmd /d /c more`,
/// verified directly against a real `more.com` to block on stdin and exit 0 once it closes (`/d`
/// skips `AutoRun`, matching `job_object`'s own `blocked_child` test fixture). Unix: `sh -c 'cat
/// >/dev/null'`.
#[cfg(windows)]
fn agent_stub_command() -> (PathBuf, Vec<String>) {
    (
        PathBuf::from("cmd.exe"),
        vec!["/d".to_owned(), "/c".to_owned(), "more".to_owned()],
    )
}

/// See the `#[cfg(windows)]` twin above.
#[cfg(not(windows))]
fn agent_stub_command() -> (PathBuf, Vec<String>) {
    (
        PathBuf::from("sh"),
        vec!["-c".to_owned(), "cat >/dev/null".to_owned()],
    )
}

#[cfg(test)]
mod test_window_fixture_tests {
    use crate::hooks::settings_file::process_is_alive;
    use crate::rail::repo::canonical_repo_path;
    use crate::settings::store::Settings;
    use crate::test_support::{open_test_app, open_test_app_with_settings};
    use crate::work_surface::agents::ProcessKind;
    use gpui::TestAppContext;
    use std::time::Duration;

    #[gpui::test]
    fn open_test_app_focuses_the_repository_it_is_given(cx: &mut TestAppContext) {
        let repo = test_support::seed_repo();

        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.focused_repo_path()),
            canonical_repo_path(repo.path()),
            "the fixture must open on the caller's repository, not one remembered on disk"
        );
    }

    #[gpui::test]
    fn open_test_app_with_settings_carries_the_settings_it_is_given(cx: &mut TestAppContext) {
        let repo = test_support::seed_repo();
        let mut settings = Settings::default();
        settings.appearance.editor_font_size = 21.0;

        let (app, cx) = open_test_app_with_settings(cx, repo.path().to_path_buf(), settings, None);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.settings.appearance.editor_font_size),
            21.0,
            "a test that passes settings in must see them, or it is asserting on defaults"
        );
    }

    /// GitHub issue #530's own regression: a `ui`-tier test spawning `ProcessKind::claude()`
    /// must never leave a real OS process behind once the app that spawned it is gone, on
    /// either platform - whether that agent CLI happens to be installed on this machine or not,
    /// since [`open_test_app`] never lets a spawn reach it in the first place.
    #[gpui::test]
    fn dropping_a_test_app_leaves_no_spawned_agent_process_behind(cx: &mut TestAppContext) {
        let repo = test_support::seed_repo();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::claude(),
                repo.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();

        let pid = app.read_with(cx, |app, cx| {
            app.agents
                .iter()
                .find(|agent| agent.id == id)
                .and_then(|agent| agent.pane.read(cx).pid())
        });
        let pid = pid.expect(
            "the stub spawned in place of a real `claude` must still have a real, live pid",
        );

        // GPUI only releases an entity whose last handle was dropped during `flush_effects`,
        // which runs inside an app update - dropping `app` and letting the test end is not
        // enough on its own (see `crate::terminal::pane`'s `pty_pane_fixtures::release`, which
        // this mirrors), and closing the window is what drops the `AdeApp` root view along with
        // every agent pane it owns.
        drop(app);
        for window in cx.windows() {
            let _ = window.update(cx, |_, window, _| window.remove_window());
        }
        cx.run_until_parked();

        assert!(
            test_support::wait_until(Duration::from_secs(5), || !process_is_alive(pid)),
            "a real spawned agent process must not outlive the test app that spawned it"
        );
    }
}
