//! Reversible worktree-level actions (Revision R10): everything about one feature, in
//! one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the code that drives it against a live window:
//!
//! - [`undo`] - the pure command-pattern undo/redo stack: already-computed outcomes and a
//!   cursor over them, so push/undo/redo/clear-redo-on-new-action semantics are directly
//!   `#[test]`-able without a window or a real git repository.
//! - [`flow`] - the real `wt_core::undo::*` background calls behind the work surface
//!   footer's `Keep all`/`Discard worktree` buttons and the app-wide `Undo`/`Redo`
//!   actions, as `impl AdeApp` methods.
//!
//! `flow` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::root::*;
use crate::work_surface::sessions::SessionId;
#[cfg(test)]
use crate::work_surface::sessions::SessionKind;
use gpui::{Context, Window};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

pub mod undo;

pub(crate) mod flow;
