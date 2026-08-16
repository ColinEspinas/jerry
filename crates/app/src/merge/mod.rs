//! Surface D, the merge/conflict-resolution flow: everything about one feature, in one
//! folder.

use crate::code_surface::code_view;
use crate::code_surface::edit_buffer;
use crate::merge::state as merge;
use crate::root::*;
use crate::theme;
#[cfg(test)]
use crate::work_surface::agents::ProcessKind;
use crate::work_surface::agents::{Agent, AgentId};
use crate::work_surface::state as work_surface;
use gpui::{
    div, font, prelude::*, px, rems, uniform_list, ClickEvent, Context, Empty, FocusHandle, Pixels,
    Window,
};
use std::ops::Range;
use std::path::{Path, PathBuf};
use wt_core::merge::{ConflictHunk, ConflictSegment, ConflictedPath};

pub mod state;

pub(crate) mod editing;
pub(crate) mod flow;
pub(crate) mod render;
