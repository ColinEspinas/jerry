//! The app's one shared menu (GitHub issue #290): the popover every "list of actions" surface
//! draws, and the pure model behind it.

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
