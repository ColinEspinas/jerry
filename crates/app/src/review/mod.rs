//! The agent **review** surface (GitHub issue #225, "Separate diffs for git and agents") -
//! everything about this feature, in one folder.
//!
//! ## The distinction this folder exists to draw
//!
//! The maintainer's report: *"Right now there is a confusion between agents diff review and git
//! diffs. We need to separate both concepts."* And, when asked to clarify: *"displaying a git
//! diff is not the same as displaying an agent diff for review, those are different concepts."*
//!
//! They are separated here by **base point and lifetime**, not by filtering one list into two:
//!
//! - A **git diff** (`crate::sidebar::changes`, the File/Diff toggle, `wt_core::diff::
//!   diff_against_base`) answers *how does this worktree differ from the merge-base with the
//!   default branch*. It's a property of the branch. Same answer regardless of who made the
//!   changes, and it only moves when the branch does.
//! - A **review** (this folder, `wt_core::review`) answers *what has changed since the point I
//!   last looked*. Its base is a snapshot of the working tree taken at a moment in time, and it
//!   advances only when the user explicitly says `Mark reviewed`.
//!
//! An agent spawned into a worktree whose branch already diverged from `main` has a big git diff
//! and an empty review. Both answers are correct; before this, only the first one existed, and it
//! was being presented as if it answered the second - which is the reported confusion, exactly.
//!
//! Downstream of the base point, everything is shared: a review diff is a real
//! `wt_core::diff::WorktreeDiff`, rendered by the very same
//! `crate::code_surface::AdeApp::render_diff_file_detail` the git side uses. There is one diff
//! renderer in this app, not two.
//!
//! ## Vocabulary
//!
//! This surface never says a bare "diff" - it says *review*, *unreviewed*, *mark reviewed*,
//! *since*. The git-side Changes sidebar and File/Diff toggle keep their existing words
//! untouched. `state`'s own `no_review_wording_anywhere_says_a_bare_diff` test pins this.
//!
//! ## The single-agent gate
//!
//! Real per-agent attribution doesn't exist yet, so in a worktree with more than one open agent
//! an agent's review would honestly include changes it didn't make. The entire review surface is
//! therefore held back for multi-agent worktrees - see
//! `crate::work_surface::agents::Agents::count_for_cwd` for the gate itself and why it's checked
//! at *display* time while baselines are still captured for every agent.
//!
//! ## Layout
//!
//! Split the way every feature folder in this crate is split (see `crate::graph_view`'s own docs
//! for the convention):
//!
//! - [`state`] - pure, GPUI-free types and wording: what a baseline is, what a review holds, the
//!   persisted key, and the exact words the tab header says.
//! - [`baseline_state`] - the real on-disk sibling file next to `settings.toml`.
//! - [`flow`] - the `impl AdeApp` glue: capturing a baseline at spawn, loading a review off the
//!   UI thread, `Mark reviewed`, and releasing a baseline's ref when an agent closes.
//! - [`render`] - the real GPUI surface: the tab strip entry, the tab body, and the open/close/
//!   focus discipline copied from the git graph tab.
//!
//! `flow`/`render` glob-import this module (`use super::*`), which is why the shared imports they
//! need live here rather than at the top of each file - the same convention `crate::root`
//! established for its own submodules.

use crate::code_surface::code_view;
use crate::root::*;
use crate::sidebar::changes;
use crate::theme;
use crate::work_surface::agents::{AgentId, AgentKind, ProcessKind};
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, App, ClickEvent, Context, Window};
use std::path::{Path, PathBuf};

pub mod baseline_state;
pub(crate) mod flow;
pub(crate) mod render;
pub mod state;
