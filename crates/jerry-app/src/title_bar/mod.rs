//! The title-bar band: everything about one feature, in one folder.

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
