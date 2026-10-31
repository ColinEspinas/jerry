//! Per-provider rate-limit budget: the agent pane's `claude 5h ▓░░░░░░ 19% 7d ▓▓▓▓░░░ 60%`
//! readout and the popover behind it (GitHub issue #294).

use crate::root::menus;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub mod fetch;

pub mod state;

pub(crate) mod flow;

pub(crate) mod render;
