//! Test-only fixtures shared by this folder's own test modules.
//!
//! Owns exactly one thing: handing a test a worktree root the app will agree with. Anything
//! git-, wait- or process-shaped belongs in `crates/test-support`, and anything that opens a
//! window in `crate::test_support`.

use gpui::VisualTestContext;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A real temporary directory whose [`Self::path`] is the root to hand
/// [`crate::test_support::open_test_app`].
///
/// The path is canonicalized because `AdeApp` canonicalizes the repo root it is given
/// ([`crate::rail::repo::canonical_repo_path`]) and then keys `lsp_clients` by an exact
/// `(PathBuf, &str)` pair and `edit_buffers`/`open_files` by `path.strip_prefix(file_tree_root)`.
/// On macOS `std::env::temp_dir()` is itself behind a symlink (`/var` -> `/private/var`), so a
/// fixture that builds paths from `tempfile::TempDir::path()` verbatim produces keys that never
/// match the ones the app resolved.
pub(in crate::lsp) struct TempRepo {
    /// Held for its `Drop`; the directory is removed when this value is.
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl TempRepo {
    pub(in crate::lsp) fn path(&self) -> &Path {
        &self.root
    }
}

pub(in crate::lsp) fn temp_repo() -> TempRepo {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize tempdir");
    TempRepo { _dir: dir, root }
}

/// `test_support::wait_until`'s GPUI-driven sibling, for the state that only advances after a
/// real render plus a `run_until_parked` between polls — a real server's `publishDiagnostics`
/// arrives on `lsp_core`'s own OS reader thread, outside GPUI's scheduler entirely.
///
/// The shared helper cannot serve these callers: its condition closure takes no arguments, and
/// theirs needs the `VisualTestContext` this hands back each time.
pub(in crate::lsp) fn wait_until_parked(
    cx: &mut VisualTestContext,
    deadline: Duration,
    mut attempt: impl FnMut(&mut VisualTestContext) -> bool,
) -> bool {
    test_support::wait_until(deadline, || attempt(cx))
}
