//! The Settings surface (Zone 4): everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free logic
//! separate from the `gpui::Div`-building code that draws it, so the former stays directly
//! `#[test]`-able without a live window:
//!
//! - [`state`] - pure page/nav/row model (which pages exist, what each row shows, real
//!   `$PATH` agent/language-server detection). No `gpui::Window`.
//! - [`store`] - the persisted `~/.config/jerry/settings.toml` file: load, save, and the
//!   read-only JSON alternate view.
//! - [`custom_theme`] - user-authored themes loaded from `~/.config/jerry/themes/*.toml`
//!   (GitHub issue #5): the real file format, validation, and import/export. No `gpui::Window`.
//! - [`render`] - the real GPUI rendering and interaction for every settings page, as
//!   `impl AdeApp` methods.
//! - [`widgets`] - the row-control widget shapes (`toggle`/`stepper`/`choice`) shared by the
//!   pages `store::ConfigPage` covers.
//!
//! `render`/`widgets` glob-import this module (`use super::*`), which is why the shared
//! imports every settings file needs live here rather than being repeated in each one -
//! the same convention `crate::root` established for its own submodules.

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
#[cfg(test)]
use crate::work_surface::agents::AgentKind;
use crate::work_surface::state as work_surface;

pub mod builtin_themes;
pub mod custom_theme;
pub mod state;
pub mod store;
pub mod theme_file_format;
pub mod vscode_theme;

pub(crate) mod render;
pub(crate) mod widgets;
