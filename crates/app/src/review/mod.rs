//! The agent **review** surface (GitHub issue #225, "Separate diffs for git and agents") -
//! everything about this feature, in one folder.

use crate::code_surface::code_view;
use crate::root::*;
use crate::sidebar::changes;
use crate::theme;
use crate::work_surface::agents::{AgentId, AgentKind, ProcessKind};
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, App, ClickEvent, Context, Window};
use std::path::{Path, PathBuf};

pub mod baseline_state;
pub(crate) mod flow;
pub(crate) mod render;
pub mod state;
