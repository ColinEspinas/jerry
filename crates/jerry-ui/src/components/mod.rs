//! Reusable, `Theme`-driven `RenderOnce` components. Each one takes a [`crate::theme::Theme`] by
//! value, carries a real `id` (painted as a real `debug_selector` for `VisualTestContext::
//! debug_bounds`, unless a caller overrides it - see each component's own `debug_selector`
//! method), and calls no `jerry-git`/`jerry-pty`/`jerry-lsp` API - it only ever draws what its
//! caller hands it.

mod badge;
mod banner;
mod button;
mod divider;
mod icon_button;
mod list_row;

pub use badge::{Badge, BadgeTone};
pub use banner::{Banner, BannerShape, BannerVariant};
pub use button::{Button, ButtonVariant};
pub use divider::{Divider, DividerOrientation};
pub use icon_button::IconButton;
pub use list_row::{ListRow, ListRowDirection, ListRowWidth};
