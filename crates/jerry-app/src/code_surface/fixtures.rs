//! Test-only fixtures shared by this folder's own test modules.
//!
//! Owns exactly one thing: handing a test a repository root the app will agree with. Anything
//! git-, wait- or process-shaped belongs in `crates/test-support` instead, and anything that
//! opens a window in `crate::test_support`.

use gpui::VisualTestContext;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A real temporary directory whose [`Self::path`] is the root to hand
/// [`crate::test_support::open_test_app`].
///
/// The path is canonicalized because `AdeApp` canonicalizes the repo root it is given
/// ([`crate::rail::repo::canonical_repo_path`]) and then keys `edit_buffers`/`open_files` by
/// `path.strip_prefix(file_tree_root)`. On macOS `std::env::temp_dir()` is itself behind a
/// symlink (`/var` -> `/private/var`), so a fixture that builds file paths from
/// `tempfile::TempDir::path()` verbatim produces paths that cannot be stripped against the root
/// the app resolved, and every worktree-relative lookup in the test then silently misses.
pub(in crate::code_surface) struct TempRepo {
    /// Held for its `Drop`; the directory is removed when this value is.
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl TempRepo {
    pub(in crate::code_surface) fn path(&self) -> &Path {
        &self.root
    }

    /// Writes `contents` to a `self`-relative `name`, creating parent directories, and returns
    /// the absolute path.
    pub(in crate::code_surface) fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent directories");
        }
        std::fs::write(&path, contents).expect("write fixture file");
        path
    }
}

pub(in crate::code_surface) fn temp_repo() -> TempRepo {
    let dir = tempfile::tempdir().expect("tempdir");
    // `dunce`, matching `canonical_repo_path` - std's Windows `\\?\` spelling breaks git
    // (GitHub issue #467).
    let root = dunce::canonicalize(dir.path()).expect("canonicalize tempdir");
    TempRepo { _dir: dir, root }
}

/// `test_support::wait_until`'s GPUI-driven sibling, for the `external`-tier tests that poll a
/// real language server: re-runs `attempt` until it reports `true` or `deadline` elapses, and
/// reports which - so the caller writes its own assertion message, exactly as the shared helper
/// does.
///
/// The shared helper cannot serve these callers: its condition closure takes no arguments, and
/// the state they poll only advances after a real render plus a `run_until_parked` that has to
/// happen *between* polls, on the `VisualTestContext` this hands back each time.
pub(in crate::code_surface) fn wait_until_parked(
    cx: &mut VisualTestContext,
    deadline: Duration,
    mut attempt: impl FnMut(&mut VisualTestContext) -> bool,
) -> bool {
    test_support::wait_until(deadline, || attempt(cx))
}
