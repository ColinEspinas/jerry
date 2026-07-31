//! The command palette (⌘P): everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free logic
//! separate from the `gpui::Div`-building code that draws it:
//!
//! - [`state`] - pure candidate/scope/filter model: which commands exist, how a query is
//!   matched and ranked, which group a row belongs to. No `gpui::Window`, so the matching
//!   rules are directly `#[test]`-able.
//! - [`render`] - the real GPUI overlay, keyboard navigation and command dispatch, as
//!   `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use gpui::{div, font, prelude::*, px, App, BoxShadow, ClickEvent, Context, KeyDownEvent, Window};
use wt_core::diff::{DiffFile, FileChangeStatus};

use crate::keymap::{self, WindowControlsStyle};
use crate::palette::state as palette;
use crate::root::*;
use crate::sidebar::changes;
use crate::sidebar::file_tree::{self, LangChip};
use crate::theme;
use crate::work_surface::sessions::SessionKind;
use crate::work_surface::state as work_surface;

pub mod state;

pub(crate) mod render;
