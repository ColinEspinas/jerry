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
//! - [`rebase`]/[`rebase_render`] - GitHub issue #242 phase B: the graph pane's own interactive-
//!   rebase mode (state/mutation glue and rendering respectively), entered from the row `⋯`
//!   menu's "Interactive rebase from here…" row - see [`rebase`]'s own module docs.
//!
//! ## Scope
//!
//! Phase (a) shipped read-only. Phase (c) (GitHub issue #1's own "push (force with lease,
//! force, no force)"/"pull") wired the toolbar's Fetch/Pull and the Push `▾` menu's
//! Push/Force-with-lease/Force rows to real `wt_core::remote` calls
//! (`AdeApp::request_graph_fetch`/`request_graph_pull`/`request_graph_push`) - see that
//! module's own docs for the fetch/pull/push implementations and
//! [`state::GraphTabState::push_force_confirm_armed`] for the real two-click confirmation the
//! two force variants require. A later pass wired the row `⋯` menu's Apply group (Cherry-pick/
//! Revert/Rebase onto this commit, `wt_core::rewrite`); GitHub issue #241 wired its Branch/Reset
//! groups too (`Check out`/`Create branch here`/Soft-Mixed-Hard reset, `wt_core::checkout`) - see
//! [`state::GraphTabState::hard_reset_confirm_armed`] for Hard reset's own two-click
//! confirmation, the same discipline `push_force_confirm_armed` already established, and
//! [`state::GraphCreateBranchPrompt`] for "Create branch here"'s small, hand-rolled branch-name
//! input; and GitHub issue #242 phase B wired "Interactive rebase from here…" to the real
//! [`rebase`] mode. Only "Start agent from this commit" is still rendered **disabled** - it
//! needs a new-worktree-creation entry point this app deliberately has none of yet (every "add
//! worktree"/"add repo" entry point stays out until a real design lands, per Revision R12) -
//! using `crate::work_surface::render::render_dropdown_menu_row`'s existing `enabled: false`
//! treatment, with no `.on_click` attached. Agent-to-commit correlation (which agent
//! authored a commit) is a
//! separate, later feature too; a first draft carried an always-empty per-commit agent column
//! for it, but `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §6.2 removed that
//! column outright rather than leave it honestly empty - a commit belongs to a worktree, which
//! can hold several agents, so pinning one agent's live status to a past commit was imprecise on
//! its own terms, independent of whether the phase (a) data existed to fill it in. The row list
//! also gained a real column header band (§6.1) in that same revision.
//!
//! `render` glob-imports [`state`] (`use super::*`), mirroring `crate::sidebar::render`'s own
//! convention; `rebase`/`rebase_render` do the same (each also imports the other's public
//! items directly, since they're siblings rather than parent/child).

use crate::root::*;
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub(crate) mod rebase;
pub(crate) mod rebase_render;
pub(crate) mod render;
pub(crate) mod state;

pub(crate) use state::{
    graph_lane_canvas_width, lane_color, lane_x, local_branch_dim_bg, relative_time,
};
pub(crate) use state::{GraphLoadState, GraphRightPanel, GraphRowMenu};
