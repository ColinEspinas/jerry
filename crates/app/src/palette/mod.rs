//! The command palette (⌘P): everything about one feature, in one folder.

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
use crate::work_surface::agents::ProcessKind;
use crate::work_surface::state as work_surface;

pub mod state;

pub(crate) mod render;
