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
//!   menu's "Rebase onto this commit" row - see [`rebase`]'s own module docs.
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
//! Revert, `wt_core::rewrite`, plus Rebase onto this commit); GitHub issue #241 wired its
//! Branch/Reset groups too (`Check out`/`Create branch here`/Soft-Mixed-Hard reset, `wt_core::checkout`) - see
//! [`state::GraphTabState::hard_reset_confirm_armed`] for Hard reset's own two-click
//! confirmation, the same discipline `push_force_confirm_armed` already established, and
//! [`state::GraphBranchPrompt`] for "Create branch here"'s small, hand-rolled branch-name
//! input.
//!
//! GitHub issue #241 also gave the **Branches panel's own rows** a real right-click context menu
//! ([`AdeApp::render_graph_branch_menu`]), matching VSCode's Git Graph extension's local-branch
//! menu scoped to seven actions: Checkout / Rename / Delete, Merge into current branch / Rebase
//! current branch on Branch, Push, and Copy Branch Name. It is the row `⋯` menu's structural twin
//! (same popover chrome, same rows, same scrim/occlude contract) but keyed by branch *name*
//! rather than a row index - see [`state::GraphBranchMenu`]. Two of its entries deliberately reuse
//! whole existing subsystems rather than growing second ones: "Merge into current branch" fills
//! the app's one existing `crate::merge` flow and conflict resolver
//! (`AdeApp::start_merge_from_graph_branch`), and "Rebase current branch on Branch" enters the
//! same [`rebase`] mode the row menu does, after resolving the branch to its real tip commit
//! ([`AdeApp::enter_rebase_mode_onto_branch`]). "Delete Branch" carries the same two-click
//! confirmation Hard reset does ([`state::GraphTabState::delete_branch_confirm_armed`]), and
//! "Rename Branch" reuses the very same branch-name prompt "Create branch here" opens.
//!
//! GitHub issue #241 folded the row menu's two rebase entries into one. "Rebase onto this commit"
//! was real but rode on `wt_core::rewrite::rebase_onto`'s plain `git rebase`, which leaves a
//! conflicted worktree mid-rebase with nothing in this app able to continue, skip or abort it. It
//! now opens the same `wt_core::rebase`-backed [`rebase`] mode GitHub issue #242 verified - whose
//! Planning banner carries a one-click `Start rebase` for the no-edit case - so a real stop lands
//! in that mode's existing Stopped strip with real `Continue`/`Skip`/`Abort`. See
//! [`AdeApp::enter_rebase_mode`].
//!
//! Agent-to-commit correlation (which agent
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
pub(crate) use state::{GraphBranchMenu, GraphLoadState, GraphRightPanel, GraphRowMenu};
