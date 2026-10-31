//! The Settings surface (Zone 4): everything about one feature, in one folder.

#[cfg(test)]
use std::path::PathBuf;

use std::time::Instant;

use gpui::{div, font, prelude::*, px, ClickEvent, Context, KeyDownEvent, Window};

use crate::keymap::{self, WindowControlsStyle};
use crate::keymap_overrides;
use crate::rail::state as rail;
use crate::rail::worktrees::WorktreeItem;
use crate::root::*;
use crate::settings::state::{self as settings, SettingsPage};
use crate::settings::store as settings_store;
use crate::sidebar::file_tree;
use crate::theme;
use crate::work_surface::agents::ProcessKind;
use crate::work_surface::state as work_surface;

pub mod builtin_themes;
pub mod custom_theme;
pub mod state;
pub mod store;
pub mod theme_file_format;
pub mod vscode_theme;

pub(crate) mod render;
pub(crate) mod widgets;
