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
//! [`rebase`] mode.
//!
//! GitHub issue #241 closed the last three gaps in that action set:
//!
//! - **"Start agent from this commit"** was, until then, rendered **disabled** ("not implemented
//!   yet") because it needs a real new-worktree-creation entry point, which this app deliberately
//!   had none of (every "add worktree"/"add repo" entry point stayed out until a real design
//!   landed, per Revision R12). Revision 6's own graph row-menu spec
//!   (`design_handoff_jerry_ade/revision 5/Jerry.dc.html`, `gMenuGroups`) is that design: it
//!   lists `Start session from this commit` with the subtitle **`new worktree`**, so the action
//!   now really runs `wt_core::add_worktree` rooted at the clicked commit and spawns an agent in
//!   the result - see `render::AdeApp::request_graph_start_agent_from_commit`. Per
//!   `REVISION-2026-08-14.md` §7 rule 1 ("ship the affordance with the behaviour, or ship
//!   neither") the row is no longer rendered inert.
//! - **"Merge into `<base>`"** is new: the graph owns merging a branch into its base since
//!   GitHub issue #285 made the Changes panel's Against-main section read-only. It lives in the
//!   **Branches** right panel, on the focused worktree's own `HEAD` branch row - the design's own
//!   placement rule (`STAGE-A-CHANGELOG.md` §4e: "worktree and branch verbs go where the worktree
//!   and branch state is visible") and its own rationale for this action specifically ("merge is
//!   a branch operation with preconditions; it lives in branch scope so the base, the commit
//!   count and the reason it is blocked are all in view at once"). It reuses the app's *existing*
//!   merge flow end to end (`crate::merge::flow::AdeApp::start_merge`), so a conflicted merge
//!   lands in the existing conflict resolver rather than a second one. See
//!   [`state::graph_merge_gate`] for the preconditions and their wording.
//! - **"Rebase onto this commit"** was real but rode on `wt_core::rewrite::rebase_onto`'s plain
//!   `git rebase`, which leaves a conflicted worktree mid-rebase with nothing in this app able to
//!   continue, skip or abort it. It now runs on the same `wt_core::rebase` engine GitHub issue
//!   #242 verified - an all-`pick` plan is exactly what a non-interactive rebase *is* - so a real
//!   stop lands in [`rebase`] mode's existing Stopped strip. See
//!   `render::AdeApp::request_graph_rebase_onto`.
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
pub(crate) use state::{GraphLoadState, GraphRightPanel, GraphRowMenu};
pub(crate) use state::{GraphMergeFacts, GraphMergeGate};
