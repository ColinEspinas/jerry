//! The 30px status bar: everything about one feature, in one folder.

use crate::keymap::{self};
use crate::rail::state as rail;
use crate::rail::status::Status;
use crate::root::menus;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context};

pub mod process_stats;

pub mod resources;

pub(crate) mod render;
