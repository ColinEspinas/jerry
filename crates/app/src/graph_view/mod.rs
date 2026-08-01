//! The git graph tab (GitHub issue #1, phase (a) - design handoff
//! `design_handoff_jerry_ade/revision 2/CHANGELOG.md`, 2026-07-31 entry).
//!
//! Split the way every feature folder in this crate is split - see `crate::sidebar`'s own docs
//! for the convention this mirrors:
//!
//! - [`state`] - pure, GPUI-window-free types and helpers: the tab's own UI state
//!   ([`state::GraphTabState`]), and pure formatting (relative time, lane pixel geometry, ref
//!   chip colours) that has nothing to do with drawing.
//! - [`render`] - the real GPUI surface: the tab strip entry, the toolbar, the lane canvas and
//!   row list, the row `⋯` menu, the Push `▾` menu, and the Commit/Branches right panel, plus
//!   the `impl AdeApp` glue that opens/closes/loads the tab.
//!
//! ## Scope (phase (a) only)
//!
//! This is read-only. The row `⋯` menu's Branch/Apply/Reset groups are real, visible menu rows
//! (per the design spec) but every entry that would perform a destructive git operation (check
//! out, cherry-pick, revert, rebase, reset, force-push, "start a session from this commit") is
//! rendered **disabled** - `crate::work_surface::render::render_dropdown_menu_row`'s existing
//! `enabled: false` treatment, with no `.on_click` attached - because none of those operations
//! exist in `wt_core` yet (that's a separate, later phase). Only the Copy group's entries
//! (real clipboard writes of already-loaded data) and the toolbar's read-only scope segment are
//! actually wired. Session-to-commit correlation (which agent session authored a commit) is a
//! separate, later feature too; a first draft carried an always-empty per-commit session column
//! for it, but `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §6.2 removed that
//! column outright rather than leave it honestly empty - a commit belongs to a worktree, which
//! can hold several agents, so pinning one agent's live status to a past commit was imprecise on
//! its own terms, independent of whether the phase (a) data existed to fill it in. The row list
//! also gained a real column header band (§6.1) in that same revision.
//!
//! `render` glob-imports [`state`] (`use super::*`), mirroring `crate::sidebar::render`'s own
//! convention.

use crate::root::*;
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub(crate) mod render;
pub(crate) mod state;

pub(crate) use state::{
    graph_lane_canvas_width, lane_color, lane_x, local_branch_dim_bg, relative_time,
};
pub(crate) use state::{GraphLoadState, GraphRightPanel, GraphRowMenu};
