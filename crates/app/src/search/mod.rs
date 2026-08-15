//! The right panel's **Search** tab: everything about one feature, in one folder.
//!
//! `design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §5 ("Search is a real panel") and
//! `STAGE-A-CHANGELOG.md` §4u/§4v/§4w re-scoped search from a flat list of filenames into the
//! middle tab of `Files · Search · Changes`, with the verdict this whole folder exists to satisfy:
//! **"a search result that only names a file is an index, not a result. Show the line."**
//!
//! Split the way every feature folder in this crate is split - pure, GPUI-free logic separate from
//! the `gpui::Div`-building code that draws it:
//!
//! - [`glob`] - the pure `include`/`exclude` pattern language (`**`, `*`, `?`, comma-separated).
//! - [`engine`] - the compiled matcher the three modifier buttons produce, the bounded worktree
//!   walk, the two-level result tree, and the real on-disk replace.
//! - [`state`] - the panel's own pure state: the four real inputs, which one has focus, the
//!   modifier toggles, per-file collapse, the load state, and the three-state gate the count row
//!   and body both read.
//! - [`render`] - the real GPUI panel, as `impl AdeApp` methods.
//!
//! `render` glob-imports `crate::root` for the shared `AdeApp` imports, the same convention
//! `crate::sidebar` established for its own submodules.

pub mod engine;
pub mod glob;
pub mod state;

pub(crate) mod render;
