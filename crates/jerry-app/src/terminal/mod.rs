//! Real terminal agents: everything about one feature, in one folder. `grid`/`mouse`/`osc` are
//! `jerry-term`'s own gpui-free VT100/grid engine, re-exported wholesale (`docs/architecture/
//! decisions.md` §25) rather than duplicated - it never depended on `gpui` even while it lived
//! here, and `jerry-host` shares the identical engine for its own headless per-session grid.

pub use jerry_term::{grid, mouse, osc};

pub mod links;
pub mod pane;
pub(crate) mod socket_adapter;
