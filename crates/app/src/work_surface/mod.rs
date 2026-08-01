//! Zone 2, the work surface: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`state`] - the pure mapping from already-known facts (an `AgentKind`, a `Status`, a
//!   bool) onto which colours/labels/actions a Zone 2 element shows. No `gpui::Window`.
//! - [`agents`] - agent/tab bookkeeping: which agents are open, which worktree each
//!   belongs to, and which one is active for the centre pane.
//! - [`render`] - the real GPUI tab strip, agent context bar, terminal pane
//!   header/footer and centre-pane composition, as `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::code_surface::code_view;
use crate::env_info;
use crate::keymap::{self};
use crate::merge::state as merge;
use crate::palette::state as palette;
use crate::rail::status::{self, Status};
use crate::root::*;
use crate::settings::state as settings;
use crate::sidebar::file_tree::{self};
use crate::theme;
use crate::work_surface::agents::{Agent, AgentId, AgentKind};
use crate::work_surface::state as work_surface;
use crate::worktree_history::flow as worktree_history;
use gpui::{div, font, prelude::*, px, App, BoxShadow, ClickEvent, Context, Window};
use std::path::{Path, PathBuf};

pub mod agents;
pub mod state;

pub(crate) mod render;
