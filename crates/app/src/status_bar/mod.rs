//! The 30px status bar: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free
//! logic separate from the `gpui::Div`-building code that draws it:
//!
//! - [`process_stats`] - the real per-process CPU%/memory sampling behind the
//!   `X% cpu · Y GB` readout: one shared trait with three real OS backends
//!   (`/proc` on Linux, `proc_pid_rusage` on macOS, `GetProcessTimes`/PSAPI on Windows -
//!   GitHub issue #283). No `gpui::Window`, so its parsing and its whole delta/aggregation
//!   pipeline stay directly `#[test]`-able.
//! - [`resources`] - the `repo → worktree → agent` cost tree that readout is the *sum of*
//!   (GitHub issue #293). Also GPUI-free, which is what lets "the bar readout is the sum of the
//!   tree" be a directly tested property rather than a claim about a `div` tree.
//! - [`render`] - the real GPUI left/right cluster layout, every field in it, and the Resources
//!   popover, as `impl AdeApp` methods.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it
//! needs live here rather than at the top of that file - the same convention `crate::root`
//! established for its own submodules.

use crate::keymap::{self};
use crate::rail::state as rail;
use crate::rail::status::Status;
use crate::root::menus;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context};

pub mod process_stats;

pub mod resources;

pub(crate) mod render;
