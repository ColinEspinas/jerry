//! Agent history: the sidebar's repo → worktree → run index, and the run-transcript centre tab
//! (GitHub issue #227). Everything about one feature, in one folder.
//!
//! Design sources, read directly rather than paraphrased:
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-13.md` (the whole document) as amended by
//! `REVISION-2026-08-14.md` §4/§6/§7, and `Jerry.dc.html`'s own `Run · transcript` state.
//!
//! ## The one sentence the whole feature hangs off
//!
//! Audit I8, quoted in the issue: **"Sidebar indexes; center shows one run."** History is a *list
//! you pick from*, like a file tree - not a document (§3: "Centre tabs hold things you read or
//! work in; the sidebar holds things you navigate"). So the sidebar carries every run, grouped
//! repo → worktree → run to match the rail's own hierarchy, and clicking one opens *its own*
//! transcript as a centre tab - exactly the Explorer → editor pattern.
//!
//! ## Split, like every other feature folder in this crate
//!
//! - [`model`] - pure, GPUI-free: what an outcome is, what a drift band is, how a run is worded,
//!   how the repo → worktree → run tree is built and filtered, and how a transcript is synthesised
//!   when none was stored. Directly unit-testable without a window.
//! - [`transcript_store`] - real per-run transcript files on disk, keyed by run id.
//! - [`flow`] - the `impl AdeApp` half of the data: finishing a run record when its agent really
//!   closes (capturing the transcript, the ending and the diffstat), and loading drift in the
//!   background.
//! - [`render`] - the real sidebar History body.
//! - [`tab`] - the real run-transcript centre tab: its strip entry, its body, and its footer's
//!   `Resume here` / `Start a new agent from this`.
//!
//! `render`/`tab`/`flow` glob-import this module (`use super::*`), which is why the shared imports
//! they need live here rather than at the top of those files - the same convention `crate::root`
//! and `crate::rail` established for their own submodules.
//!
//! ## What is real here, and what is derived
//!
//! Every run in this surface is a real [`crate::hooks::store::PersistedAgentStatus`] record that
//! an agent's own hooks produced - Jerry never invents a run. Its title is the first prompt its
//! human typed, its turn count is one per real `Stop`, its diffstat was measured against its own
//! review baseline at the moment it ended, and its drift is a real `git log` in its own checkout.
//! The two derived things are its **outcome** ([`model::Outcome::of`], a rule over those recorded
//! facts) and, where no transcript was captured, a short synthesised one built from that run's own
//! record - which is what §3 asks for in as many words, and never another run's output.

use crate::root::*;
use crate::theme;
use crate::work_surface::agents::{AgentId, ProcessKind};
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};
use std::collections::HashMap;
use std::path::PathBuf;

pub mod model;
pub mod transcript_store;

pub(crate) mod flow;
pub(crate) mod render;
pub(crate) mod tab;

/// Seconds since the Unix epoch, mirroring `crate::hooks::flow`'s own `unix_now` (including its
/// `unwrap_or(0)` for a clock set before 1970). Shared by all three halves of this feature so a
/// row, its transcript header and its closing line can never date the same run off two clocks.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
