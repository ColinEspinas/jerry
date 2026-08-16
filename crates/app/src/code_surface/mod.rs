//! Surface C, the code surface: everything about one feature, in one folder.

use crate::code_surface::state::{
    BlameCacheEntry, BlameLoadState, CommitMessageState, DiffLoadState, FileLoadState, HoverAnchor,
    HoverEntry, HoverStatus,
};
use crate::language;
use crate::lsp::client::LspClientState;
use crate::lsp::diagnostics as diagnostics_view;
use crate::lsp::hover as hover_view;
#[cfg(test)]
use crate::rail::worktrees;
use crate::root::*;
use crate::settings::store as settings_store;
use crate::sidebar::changes::{self};
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{
    div, font, prelude::*, px, rems, uniform_list, ClickEvent, Context, MouseButton,
    MouseDownEvent, Pixels, ScrollStrategy, Window,
};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;
use wt_core::diff::{DiffBase, DiffFile, DiffLineKind, WorktreeDiff};

pub mod blame;
pub mod code_view;
pub mod edit_buffer;
pub mod fold;
pub mod indent;
pub mod symbols;

pub(crate) mod blame_view;
pub(crate) mod diff_view;
pub(crate) mod editing;
pub(crate) mod file_view;
pub(crate) mod lsp_ui;
pub(crate) mod markdown_preview;
pub(crate) mod minimap;
pub(crate) mod render;
pub(crate) mod state;
pub(crate) mod tabs;
pub(crate) mod zoom;
