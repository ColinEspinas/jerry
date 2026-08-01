//! The 28px status bar: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`process_stats`] - the real per-process CPU%/memory sampling behind the
//!   `N agents · X% cpu · Y GB` cluster, over `/proc`. No `gpui::Window`, so its parsing
//!   stays directly `#[test]`-able.
//! - [`render`] - the real GPUI left/right cluster layout and every field in it, as
//!   `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::code_surface::code_view;
use crate::keymap::{self};
use crate::lsp::client::LspClientState;
#[cfg(test)]
use crate::lsp::diagnostics as diagnostics_view;
use crate::rail::state as rail;
use crate::root::*;
use crate::theme;
// `Agent` itself is only named inside `render::render_status_agents_cluster`'s real
// `#[cfg(target_os = "linux")]` variant (`Vec<&Agent>`) - the non-Linux twin only needs
// `AgentKind`, so a Windows/macOS build genuinely never references `Agent` here.
#[cfg(target_os = "linux")]
use crate::work_surface::agents::Agent;
use crate::work_surface::agents::AgentKind;
use gpui::{div, font, prelude::*, px, ClickEvent, Context};
#[cfg(test)]
use std::path::PathBuf;

pub mod process_stats;

pub(crate) mod render;
