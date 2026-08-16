//! Test-only fixtures shared by this folder's own test modules.
//!
//! Owns exactly one thing: handing a test a worktree root the app will agree with. Anything
//! git-, wait- or process-shaped belongs in `crates/test-support`, and anything that opens a
//! window in `crate::test_support`.

use std::path::{Path, PathBuf};

/// A real temporary directory whose [`Self::path`] is the root to hand
/// [`crate::test_support::open_test_app`].
///
/// The path is canonicalized because `AdeApp` canonicalizes the repo root it is given
/// ([`crate::rail::repo::canonical_repo_path`]) and then keys `open_files`/`edit_buffers` by
/// `path.strip_prefix(file_tree_root)`. On macOS `std::env::temp_dir()` is itself behind a
/// symlink (`/var` -> `/private/var`), so a fixture that builds paths from
/// `tempfile::TempDir::path()` verbatim produces keys that never match the ones the app resolved
/// — which is exactly how a replace-all could once overwrite a file the editor still held dirty.
pub(in crate::search) struct TempRepo {
    /// Held for its `Drop`; the directory is removed when this value is.
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl TempRepo {
    pub(in crate::search) fn path(&self) -> &Path {
        &self.root
    }
}

pub(in crate::search) fn temp_repo() -> TempRepo {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize tempdir");
    TempRepo { _dir: dir, root }
}
