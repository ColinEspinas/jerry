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
//! - [`context_menu`] - the pure model of the right-click menu (GitHub issue #19 §1): which
//!   actions each target offers, how they group, and the edge-aware geometry that keeps the
//!   popover - dividers and all - on screen.
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
use gpui::{div, font, prelude::*, px, uniform_list, ClickEvent, Context, Pixels, Window};
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

pub(crate) mod render;
pub(crate) mod tree_ops;
