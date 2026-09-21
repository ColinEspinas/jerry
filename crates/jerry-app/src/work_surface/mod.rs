//! Zone 2, the work surface: everything about one feature, in one folder.

use crate::code_surface::code_view;
use crate::env_info;
use crate::keymap::{self};
use crate::palette::state as palette;
use crate::rail::status::{self, Status};
use crate::rail::title_signal;
use crate::root::*;
use crate::settings::state as settings;
use crate::sidebar::file_tree::{self};
use crate::theme;
use crate::work_surface::agents::{Agent, AgentId, AgentKind, ProcessKind};
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, App, ClickEvent, Context, Window};
use std::path::{Path, PathBuf};

pub mod agents;
pub mod state;
pub mod tab_order_state;

pub(crate) mod render;
pub(crate) mod session;
