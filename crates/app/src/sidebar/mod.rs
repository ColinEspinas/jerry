//! Zone 3, the right sidebar: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`file_tree`] - the pure `std::fs::read_dir` walk that flattens a directory into the
//!   indented row list the Files tab shows, plus the pure indent-guide geometry that list
//!   draws.
//! - [`fold_state`] - the real, on-disk (`~/.config/jerry/file-tree-state.toml`) per-worktree
//!   record of which folders are expanded, and its atomic write path.
//! - [`changes`] - the pure mapping from `wt_core::diff` data to the Changes tab's row
//!   labels/colours/counts and the fold-marker treatment.
//! - [`render`] - the real GPUI sidebar: the Files/Changes tab switch, its virtualized row
//!   lists and their click handlers, as `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::root::*;
use crate::sidebar::file_tree::{FileTreeEntry, LangChip};
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, uniform_list, ClickEvent, Context, Pixels};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use wt_core::diff::{DiffFile, FileChangeStatus, WorktreeDiff};

pub mod changes;
pub mod file_tree;
pub mod fold_state;

pub(crate) mod render;
