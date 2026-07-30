//! Language tooling: everything the real language servers feed into this app, in one
//! folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free view
//! models separate from the code that talks to a real server and paints the result:
//!
//! - [`hover`] / [`diagnostics`] / [`completion`] - pure mappings from an `lsp_types`
//!   response onto the render model each popup/decoration needs. No `gpui::Window`, so the
//!   mapping rules stay directly `#[test]`-able against fixture responses.
//! - [`client`] - the real `lsp_core::LspClient` lifecycle: which servers exist for a
//!   worktree, spawning/evicting them, sending real requests and draining diagnostics.
//! - [`completion_popup`] - the real GPUI Completions popup over the code surface, and the
//!   actions that navigate and accept it.
//!
//! `client`/`completion_popup` glob-import this module (`use super::*`), which is why the
//! shared imports they need live here rather than at the top of each file - the same
//! convention `crate::root` established for its own submodules.

use crate::lsp::diagnostics as diagnostics_view;
#[cfg(test)]
use crate::rail::worktrees::WorktreeItem;
use crate::root::*;
use gpui::{prelude::*, Context, Window};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub mod completion;
pub mod diagnostics;
pub mod hover;

pub(crate) mod client;
pub(crate) mod completion_popup;
