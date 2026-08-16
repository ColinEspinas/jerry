//! Zone 3, the right sidebar: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`file_tree`] - the pure `std::fs::read_dir` walk that flattens a directory into the
//!   indented row list the Files tab shows, plus the pure indent-guide geometry that list
//!   draws.
//! - [`file_tree_watch`] - the real `notify`-backed filesystem watcher behind the Files tab's
//!   live refresh (GitHub issue #13) - the same "OS watch sets a flag, a `gpui` background loop
//!   polls it" split `crate::rail::worktree_watch` established for the worktree list.
//! - [`fold_state`] - the real, on-disk (`~/.config/jerry/file-tree-state.toml`) per-worktree
//!   record of which folders are expanded, and its atomic write path.
//! - [`changes`] - the pure mapping from `wt_core::diff` data to the Changes tab's row
//!   labels/colours/counts and the fold-marker treatment.
//! - [`sections`] - the pure model of the Changes tab's four stacked sections (GitHub issue
//!   #285): which scope each answers, its collapse state, its header's counts and diffstat, the
//!   run rows derived from the provenance union, and the `seen` marks that are deliberately not
//!   the staged set.
//! - [`context_menu`] - the pure model of the right-click menu (GitHub issue #19 §1): which
//!   actions each target offers and how they group. The popover itself, its row shape and the
//!   edge-aware geometry that keeps it on screen are `crate::menu`'s since GitHub issue #290
//!   promoted this menu into the app's one shared component.
//! - [`file_ops`] - the pure filesystem primitives behind those actions (issue #19 §2/§3): name
//!   validation, collision-free naming, recursive copy/move/delete, and the real OS-trash
//!   decision.
//! - [`tree_ops`] - the `impl AdeApp` glue that sequences those two: menu state, the inline
//!   name editors, the cut/copy/paste buffer, and the confirmed delete.
//! - [`render`] - the real GPUI sidebar: the Files/Changes tab switch, its virtualized row
//!   lists and their click handlers, as `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::root::*;
use crate::sidebar::file_tree::{FileTreeEntry, LangChip};
use crate::theme;
use crate::work_surface::agents::ProcessKind;
use crate::work_surface::state as work_surface;
use gpui::{
    div, font, prelude::*, px, uniform_list, ClickEvent, Context, KeyDownEvent, Pixels, Window,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use wt_core::diff::{DiffFile, FileChangeStatus, WorktreeDiff};

pub mod changes;
pub mod context_menu;
pub mod file_ops;
pub mod file_tree;
pub mod file_tree_watch;
pub mod fold_state;
pub mod sections;

pub(crate) mod render;
pub(crate) mod tree_ops;

/// Fixtures shared by this folder's own test modules.
#[cfg(test)]
pub(in crate::sidebar) mod fixtures {
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A scratch directory, plus the path every assertion about it must be written against.
    ///
    /// `AdeApp` resolves each repository path it is handed (`crate::rail::repo::canonical_repo_path`),
    /// so on macOS - where `TMPDIR` lives under `/var/folders`, a symlink to `/private/var/folders` -
    /// a test that builds expectations out of `TempDir::path()` is comparing unresolved paths
    /// against the resolved ones the app actually holds, and matches nothing.
    pub(in crate::sidebar) fn temp_root() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize tempdir");
        (dir, root)
    }
}
