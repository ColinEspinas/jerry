//! GPUI-flavoured test fixtures: opening a real `AdeApp` in a test window.
//!
//! Deliberately *not* part of `crates/test-support`, which the gpui-free core crates
//! dev-depend on (`docs/architecture/decisions.md` §1) — a Cargo feature would not be enough,
//! since workspace feature unification would pull `gpui` into their dev graph anyway. Repo,
//! git and wait fixtures with no GPUI in them belong there, not here.

use crate::root::AdeApp;
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

fn canonicalized(dir: tempfile::TempDir) -> TempRoot {
    let root = dir.path().canonicalize().expect("canonicalize tempdir");
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
    cx.add_window_view(|window, cx| {
        AdeApp::new_with_settings(Some(repo_path), true, settings, settings_path, window, cx)
    })
}

#[cfg(test)]
mod test_window_fixture_tests {
    use crate::rail::repo::canonical_repo_path;
    use crate::settings::store::Settings;
    use crate::test_support::{open_test_app, open_test_app_with_settings};
    use gpui::TestAppContext;

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
}
