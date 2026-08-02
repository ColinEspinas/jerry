//! Real GitHub-releases-backed update detection and self-update (GitHub issue #87): everything
//! about "is a newer Jerry release out, and can the user click to install it" lives in this one
//! folder, split the way every feature folder in this crate is - pure, GPUI-window-free logic
//! separate from the real `impl AdeApp` code that drives/draws it:
//!
//! - [`state`] - the pure data model ([`state::ReleaseInfo`], [`state::UpdateState`]) and the
//!   real version-comparison function, unit-tested directly with no GPUI/network involved.
//! - [`flow`] - the real `self_update`-backed background checks/downloads, and the new
//!   relaunch-and-exit mechanism this app didn't have before this issue, as `impl AdeApp`
//!   background-task methods.
//! - [`render`] - the status bar's "Update available"/"Updating…"/"Restart to update"/"Update
//!   failed" chip, as an `impl AdeApp` method pushed into
//!   `crate::status_bar::render::AdeApp::render_status_bar_left`'s segment list.
//!
//! `flow`/`render` both glob-import this module (`use super::*`), which is why the shared
//! imports they need live here rather than at the top of each file - the same convention
//! `crate::worktree_history`/`crate::status_bar` already established for their own submodules.
//!
//! ## Never intrusive on a *check* failure
//!
//! A background update *check* can fail for entirely mundane, expected reasons: offline, DNS
//! failure, GitHub's unauthenticated 60-requests/hour rate limit, a transient 5xx, a TLS
//! handshake failure. None of those are this app's problem to interrupt the user over -
//! [`flow::AdeApp::check_for_update`]'s own docs are the one real enforcement point for this
//! rule: a check failure only ever `log::warn!`s and leaves [`state::UpdateState`] exactly as it
//! was, never touching it and never calling `cx.notify()`. Only a *download* failure - after the
//! user has already clicked "Update available" - is ever surfaced as [`state::UpdateState::Failed`].

use crate::root::widgets::text_tooltip;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context};

pub(crate) mod flow;
pub(crate) mod render;
pub(crate) mod state;
