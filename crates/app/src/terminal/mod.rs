//! Real terminal sessions: everything about one feature, in one folder.
//!
//! Split by what each half is responsible for, keeping the window-free parts window-free:
//!
//! - [`pane`] - one terminal-backed pane: a real `pty-core` process, its
//!   `alacritty_terminal`-backed grid, and the GPUI element that paints it cell by cell.
//! - [`grid`] - the ANSI/VT100 grid emulation itself, over `alacritty_terminal::Term`.
//! - [`links`] - the pure `path:line[:col]` scanner over already-rendered row text, GPUI-free
//!   so its matching rules stay directly `#[test]`-able.
//!
//! Unlike the other feature folders, these three files were already self-contained top-level
//! modules with their own imports (never `use super::*`), so this module deliberately carries
//! no shared import block - only the grouping.

pub mod grid;
pub mod links;
pub mod pane;
