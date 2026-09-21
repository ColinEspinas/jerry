//! Agent history: the sidebar's repo → worktree → run index, and the run-transcript centre tab
//! (GitHub issue #227). Everything about one feature, in one folder.

use crate::root::*;
use crate::theme;
use crate::work_surface::agents::{AgentId, ProcessKind};
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};
use std::collections::HashMap;
use std::path::PathBuf;

pub mod model;
pub mod transcript_store;

pub(crate) mod flow;
pub(crate) mod render;
pub(crate) mod tab;

/// Seconds since the Unix epoch, mirroring `crate::hooks::flow`'s own `unix_now` (including its
/// `unwrap_or(0)` for a clock set before 1970). Shared by all three halves of this feature so a
/// row, its transcript header and its closing line can never date the same run off two clocks.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
