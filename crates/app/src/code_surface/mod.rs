//! Surface C, the code surface: everything about one feature, in one folder.
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-window-free logic
//! separate from the `gpui::Div`-building code that draws it - and then split again inside
//! each half, because this is the largest subsystem in the app:
//!
//! Pure (no `gpui::Window`, directly `#[test]`-able):
//! - [`code_view`] - reading a file off disk, detecting its line endings, and producing
//!   real tree-sitter syntax spans for it.
//! - [`edit_buffer`] - one open file's live edited text, cursor, selection and IME state,
//!   and every mutation that can happen to them.
//! - [`blame`] - turning a `wt_core::blame::BlameLine` into the inline label/tooltip text
//!   shown for the current line (GitHub issue #29).
//! - [`indent`] - Tab/Shift+Tab indentation resolution (tabs vs. spaces, width), including a
//!   real, minimal `.editorconfig` reader (GitHub issue #26).
//! - [`fold`] - which bracket-delimited regions of one file can be collapsed, and the
//!   visual-row <-> buffer-line translation the File view's `uniform_list` needs once some of
//!   them are (GitHub issue #202).
//!
//! GPUI-facing:
//! - [`state`] - the load-state enums the surface's own fields are typed with.
//! - [`tabs`] - opening/activating/closing file and diff tabs, and the background loads
//!   behind them.
//! - [`render`] - the surface shell: which view a tab shows, and the Diff/File toggle.
//! - [`diff_view`] - the read-only Diff view's rows, gutter, fold markers and highlight
//!   cache.
//! - [`file_view`] - the editable File view's rows, breadcrumb and status bar.
//! - [`blame_view`] - the off-thread, cached inline git blame load/render (GitHub issue #29).
//! - [`zoom`] - the editor zoom level and the rem-scoped subtree it applies through.
//! - [`lsp_ui`] - the language-server UI drawn *over* the surface: hover card, diagnostics
//!   card, and go-to-definition. (The client itself lives in `crate::lsp`.)
//! - [`editing`] - the `EntityInputHandler`/action wiring that drives [`edit_buffer`] from
//!   real keystrokes.
//! - [`minimap`] - the real reduced-scale, syntax-colored minimap to the right of the code
//!   column: draggable viewport slider, click-to-jump, and the git-diff overlay.
//!
//! Every GPUI-facing file here except [`editing`]/[`minimap`] glob-imports this module
//! (`use super::*`), which is why the shared imports they need live here rather than at the
//! top of each file - the same convention `crate::root` established for its own submodules.
//! [`editing`]/[`minimap`] are the exceptions: each needs `gpui` symbols outside the shared
//! subset this module re-exports (canvas/fill/point/size/`Bounds` for [`minimap`],
//! `EntityInputHandler`/`UTF16Selection`/etc for [`editing`]), so each keeps its own explicit
//! `use gpui::{...};` block instead.

use crate::code_surface::state::{
    BlameCacheEntry, BlameLoadState, CommitMessageState, DiffLoadState, FileLoadState, HoverAnchor,
    HoverEntry, HoverStatus,
};
use crate::keymap::{self};
use crate::language;
use crate::lsp::client::LspClientState;
use crate::lsp::diagnostics as diagnostics_view;
use crate::lsp::hover as hover_view;
#[cfg(test)]
use crate::rail::worktrees;
use crate::root::*;
use crate::settings::store as settings_store;
use crate::sidebar::changes::{self};
use crate::theme;
use crate::work_surface::state as work_surface;
use gpui::{
    div, font, prelude::*, px, rems, uniform_list, ClickEvent, Context, MouseButton,
    MouseDownEvent, Pixels, ScrollStrategy, Window,
};
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;
use wt_core::diff::{DiffBase, DiffFile, DiffLineKind, WorktreeDiff};

pub mod blame;
pub mod code_view;
pub mod edit_buffer;
pub mod fold;
pub mod indent;
pub mod symbols;

pub(crate) mod blame_view;
pub(crate) mod diff_view;
pub(crate) mod editing;
pub(crate) mod file_view;
pub(crate) mod lsp_ui;
pub(crate) mod markdown_preview;
pub(crate) mod minimap;
pub(crate) mod render;
pub(crate) mod state;
pub(crate) mod tabs;
pub(crate) mod zoom;
