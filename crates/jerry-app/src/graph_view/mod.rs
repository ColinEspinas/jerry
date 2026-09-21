//! The git graph tab (GitHub issue #1, phase (a)).

use crate::root::*;
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{div, font, prelude::*, px, ClickEvent, Context, Window};

pub(crate) mod rebase;
pub(crate) mod rebase_render;
pub(crate) mod render;
pub(crate) mod state;

pub(crate) use state::{
    graph_branch_merge_gate, GraphBranchMenu, GraphBranchMergeFacts, GraphLoadState,
    GraphRightPanel, GraphRowMenu,
};
pub(crate) use state::{
    graph_lane_canvas_width, lane_color, lane_x, local_branch_dim_bg, relative_time,
};
