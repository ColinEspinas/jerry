//! The app's one shared menu (GitHub issue #290): the popover every "list of actions" surface
//! draws, and the pure model behind it.
//!
//! `design_handoff_jerry_ade/revision 5/STAGE-A-CHANGELOG.md` §4t states the reason for the
//! folder existing at all, verbatim: "Two requests, one build: right-click actions on rail rows,
//! and History moved into an overflow. Both are 'a list of actions', so they are **one menu
//! component** - 206 wide, 24px rows, keycaps right-aligned, destructive rows in `#c4726d` on a
//! `#2a1719` hover - rather than two idioms that drift."
//!
//! - [`model`] - the pure data: [`model::MenuEntry`], grouping into separated
//!   [`model::MenuRow`]s, the popover's real painted height, and the two anchor rules (pointer
//!   for a right-click, a button's own rect for the `⋯` overflow). No `gpui::Window`, so every
//!   row set and every edge flip is testable without one.
//! - [`render`] - the single real popover: one occluding scrim, one panel, one row renderer, as
//!   `impl AdeApp` methods.
//!
//! ## Who draws through it
//!
//! The file tree's right-click menu (`crate::sidebar::context_menu`, where all of this was
//! written first, for GitHub issue #19), the rail's worktree- and agent-row menus, and the
//! sidebar strip's `⋯` overflow (`crate::rail::menu`). Each of those owns only its *rows* - what
//! a menu looks like, how it flips at a window edge, and how a disabled row explains itself are
//! answered here, once.
//!
//! ## Where it renders
//!
//! At the root, never inside the panel it was opened from. `REVISION-2026-08-14.md` §4, verbatim:
//! "All menus render outside the scrolling list. Inside it they are clipped by the scroller and
//! scroll away from their anchor." And `STAGE-A-CHANGELOG.md` §4w's generalisation: "an overlay
//! anchored in viewport coordinates must live at the root. If it is nested in a panel, every
//! property of that panel - its scroll, its clip, its mount condition - becomes a bug in the
//! overlay." Every caller therefore renders its menu from `crate::root::AdeApp::render`'s own
//! overlay list, and registers it in [`crate::root::menus::MenuSurface`] so the app's
//! one-menu-at-a-time invariant holds.
//!
//! `render` glob-imports this module (`use super::*`), which is why the shared imports it needs
//! live here rather than at the top of that file - the same convention `crate::root` established
//! for its own submodules.

use crate::keymap;
use crate::root::widgets::{
    menu_popover_chrome, render_keycap_row, render_menu_group_divider, text_tooltip, KeycapSize,
};
use crate::root::*;
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub mod model;
pub(crate) mod render;
