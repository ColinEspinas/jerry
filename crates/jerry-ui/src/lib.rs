//! Jerry's design-system crate: semantic theming tokens plus a small set of reusable GPUI
//! components. The second (and, per `CLAUDE.md`, only other) crate in this workspace allowed to
//! depend on `gpui` - `jerry-app` constructs a [`theme::Theme`] from its own resolved palette and
//! passes it into every component here; this crate never reads `jerry-app`'s state, settings, or
//! any of the other core crates. See `docs/architecture/decisions.md` §27 for the scope decision.

#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_methods)
)]

pub mod components;
mod handlers;
pub mod schema;
pub mod state_style;
pub mod theme;

pub use components::{
    Badge, BadgeTone, Banner, BannerVariant, Button, ButtonVariant, Divider, IconButton, ListRow,
};
pub use state_style::{ElementState, StateStyle, StyleSet};
pub use theme::Theme;
