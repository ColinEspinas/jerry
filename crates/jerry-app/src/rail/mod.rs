//! Zone 1, the agent rail: everything about one feature, in one folder.

use crate::keymap;
use crate::rail::state::{
    self as rail, AgentRow, RepoGroup, RepoWorktrees, WorktreeEntry, WorktreeNote, WorktreeRow,
};
use crate::rail::status::Status;
use crate::rail::strip as rail_strip;
use crate::rail::worktrees::WorktreeItem;
use crate::root::*;
use crate::status_bar::process_stats;
use crate::theme;
use crate::work_surface::agents::ProcessKind;
use gpui::{div, font, prelude::*, px, App, ClickEvent, Context, KeyDownEvent, Window};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

pub mod menu;
pub mod repo;
pub mod state;
pub mod status;
pub mod strip;
pub mod title_signal;
pub mod worktree_watch;
pub mod worktrees;

pub(crate) mod menu_render;
pub(crate) mod render;
pub(crate) mod strip_render;
