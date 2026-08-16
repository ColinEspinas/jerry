//! Per-provider rate-limit budget: the agent pane's `claude 5h ▓░░░░░░ 19% 7d ▓▓▓▓░░░ 60%`
//! readout and the popover behind it (GitHub issue #294,
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §2,
//! `STAGE-A-CHANGELOG.md` §4c/§4t/§4u′).

use crate::root::menus;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub mod fetch;

pub mod state;

pub(crate) mod flow;

pub(crate) mod render;
