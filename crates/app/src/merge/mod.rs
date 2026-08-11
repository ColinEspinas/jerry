//! Surface D, the merge/conflict-resolution flow: everything about one feature, in one
//! folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`state`] - the pure conflict/segment/choice model: which side a hunk resolves to,
//!   what a resolved file's text becomes. No `gpui::Window`, so it stays directly
//!   `#[test]`-able.
//! - [`flow`] - the real background `wt_core::merge` calls and the surface's own
//!   open/advance/abort state machine, as `impl AdeApp` methods.
//! - [`editing`] - the whole-file hand-edit buffer wiring for a conflict the side-picker
//!   can't resolve (Revision R8.5c).
//! - [`render`] - the real GPUI conflict view, side pickers and footer.
//!
//! `flow`/`editing`/`render` glob-import this module (`use super::*`), which is why the
//! shared imports they need live here rather than at the top of each file - the same
//! convention `crate::root` established for its own submodules.

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
