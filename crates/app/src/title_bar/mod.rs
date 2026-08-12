//! The title-bar band: everything about one feature, in one folder.
//!
//! Unlike the pure-logic/rendering split every other feature folder uses, this one is a
//! rendering subsystem throughout (there is no window-free decision logic behind a title
//! bar); it is split by what a reader is actually looking for:
//!
//! - [`render`] - the band itself: the macOS traffic-light cluster, the Windows/Linux
//!   caption buttons, and the left/right cluster composition.
//! - [`menu`] - the Windows/Linux `File Edit View Agent Help` dropdowns: which rows each
//!   one offers and what real `AdeApp` method each row calls.
//! - [`menu_model`] - the pure-data command model ([`menu_model::MenuCommand`],
//!   [`menu_model::MenuRow`]) both [`menu`]'s popover and `native_menu`'s real macOS menu
//!   render from, so the two can never silently offer two different command sets (GitHub issue
//!   #235).
//! - `native_menu` (macOS only) - the real `NSApp.mainMenu`, built from [`menu_model`] and
//!   installed via `gpui::App::set_menus` in `crate::run`.
//!
//! Both glob-import this module (`use super::*`), which is why the shared imports they need
//! live here rather than at the top of each file - the same convention `crate::root`
//! established for its own submodules.

#[cfg(test)]
use crate::code_surface::edit_buffer;
use crate::keymap;
#[cfg(test)]
use crate::keymap::WindowControlsStyle;
#[cfg(test)]
use crate::palette::state as palette;
use crate::rail::state::{self as rail, AgentRow};
use crate::rail::status::Status;
#[cfg(test)]
use crate::rail::worktrees::WorktreeItem;
use crate::root::*;
#[cfg(test)]
use crate::settings::state as settings;
use crate::theme;
use crate::title_bar::menu::TitleMenu;
#[cfg(test)]
use crate::work_surface::agents::AgentId;
use crate::work_surface::agents::ProcessKind;
use crate::work_surface::state as work_surface;
use crate::worktree_history::flow as worktree_history;
#[cfg(test)]
use gpui::Pixels;
use gpui::{
    div, font, prelude::*, px, App, ClickEvent, Context, MouseButton, Window, WindowControlArea,
};
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::time::Duration;

pub(crate) mod menu;
pub(crate) mod menu_model;
#[cfg(target_os = "macos")]
pub(crate) mod native_menu;
pub(crate) mod render;
