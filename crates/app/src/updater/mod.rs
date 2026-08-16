//! Real GitHub-releases-backed update detection and self-update (GitHub issue #87): everything
//! about "is a newer Jerry release out, and can the user click to install it" lives in this one
//! folder, split the way every feature folder in this crate is - pure, GPUI-window-free logic
//! separate from the real `impl AdeApp` code that drives/draws it:

use crate::root::widgets::text_tooltip;
use crate::root::*;
use crate::theme;
use gpui::{div, font, prelude::*, px, ClickEvent, Context};

pub(crate) mod flow;
pub(crate) mod render;
pub(crate) mod state;
