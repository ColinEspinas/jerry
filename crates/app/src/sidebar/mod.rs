//! Zone 3, the right sidebar: everything about one feature, in one folder.

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
