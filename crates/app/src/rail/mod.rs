//! Zone 1, the session rail: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`repo`] - one added git repository (`Repo`/`RepoId`), the rail's group level
//!   (Revision R12 Phase 0), plus its own crash-safe `~/.config/jerry/repos.toml`
//!   persistence, mirroring [`crate::sidebar::fold_state`]'s pattern.
//! - [`state`] - the pure row model: one row per worktree, aggregating every session open
//!   in it, plus the filter/grouping rules. No `gpui::Window`.
//! - [`worktrees`] - maps `wt-core`'s raw worktree list into that row model's input shape.
//! - [`status`] - derives a session's `Status` ("who needs me") from already-read process
//!   signals; the rail's whole reason for existing, and shared with the tab strip and
//!   status bar that surface the same answer.
//! - [`render`] - the real GPUI rail, its rows, hover affordances and click handlers, as
//!   `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::keymap;
use crate::rail::state::{
    self as rail, RepoGroup, RepoWorktrees, SessionRow, WorktreeEntry, WorktreeNote, WorktreeRow,
};
use crate::rail::status::Status;
#[cfg(test)]
use crate::rail::worktrees::WorktreeItem;
use crate::root::*;
use crate::status_bar::process_stats;
use crate::theme;
use crate::work_surface::sessions::SessionKind;
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, App, ClickEvent, Context, KeyDownEvent, Window};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

pub mod repo;
pub mod state;
pub mod status;
pub mod worktrees;

pub(crate) mod render;
