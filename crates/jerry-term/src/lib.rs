//! The VT100/grid engine `jerry-app`'s own rendered `TerminalPane` and `jerry-host`'s headless
//! per-session grid (`docs/architecture/decisions.md` §25) both feed real pty bytes through -
//! extracted from `jerry-app` rather than duplicated, since it was already `gpui`-free in its own
//! right (only `jerry-app`'s consumption of it, in `crate::terminal::pane`, ever touched `gpui`).
//! Zero `gpui` dependency, matching `jerry-git`/`jerry-pty`/`jerry-lsp` - see `CLAUDE.md`'s
//! architecture section.

// Only production code is held to `unwrap_used`/`expect_used` (`CLAUDE.md`).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod grid;
pub mod mouse;
pub mod osc;
