//! The worktree footer's real "keep all changes"/"discard worktree" actions: everything about
//! this feature, in one folder.
//!
//! [`flow`] holds the real `wt_core::undo::*` background calls behind the work surface footer's
//! `Keep all`/`Discard worktree` buttons, as `impl AdeApp` methods. It glob-imports this module
//! (`use super::*`), which is why the shared imports it needs live here rather than at the top
//! of that file - the same convention `crate::root` established for its own submodules.

use crate::root::*;
use crate::work_surface::agents::AgentId;
#[cfg(test)]
use crate::work_surface::agents::ProcessKind;
use gpui::{Context, Window};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

pub(crate) mod flow;
