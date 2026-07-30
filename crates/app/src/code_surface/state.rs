//! The load-state enums Surface C's own `AdeApp` fields are typed with - one place to
//! look for "what states can an open diff / open file / hover actually be in".

use super::*;

/// The outcome of the most recent (or in-flight) `diff_against_base` call for
/// [`AdeApp::diff_root`]. Kept separate from [`DiffBase`] so "still computing" is a first-class
/// state, distinct from an empty/default value that could be mistaken for "no changes".
pub(crate) enum DiffLoadState {
    Loading,
    Loaded(DiffBase),
    Error(String),
}

/// The outcome of the most recent (or in-flight) `code_view::load_file` call for whichever path
/// [`AdeApp::render_file_view`] most recently asked to load. Mirrors [`DiffLoadState`]'s shape:
/// `load_file` does the same class of blocking I/O (`std::fs::read`, plus a `tree-sitter` parse
/// for `.rs` files) and must never run on the GPUI foreground thread.
///
/// Kept separate from [`AdeApp::file_view_cache`] rather than folded into an
/// `Option<Result<ParsedFile, String>>` there, so a fresh load for a newly opened file doesn't
/// overwrite (and blank) whatever was last successfully shown while it's still in flight.
#[derive(Debug)]
pub(crate) enum FileLoadState {
    Idle,
    Loading(PathBuf),
    Error(PathBuf, String),
}

/// The state of one in-flight or completed click-triggered `textDocument/hover` request; see
/// [`AdeApp::hover`]'s docs for the caching discipline this backs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HoverEntry {
    /// The absolute path of the file the hovered symbol is in; `render_file_view` only shows
    /// [`Self::status`] when it matches the file currently open.
    pub(in crate::code_surface) path: PathBuf,
    /// 1-based line number (matching [`AdeApp::code_cursor`]'s convention); half of this entry's
    /// cache key along with [`Self::byte_range`].
    pub(in crate::code_surface) line_number: usize,
    /// Byte range, within the line's text, of the clicked token - the span
    /// [`crate::code_surface::file_view::render_file_view_line`] underlines with `theme::syntax::HOVER_UNDERLINE`,
    /// and the other half of the cache key.
    pub(in crate::code_surface) byte_range: Range<usize>,
    /// The LSP `Position` this request was/will be sent with, kept alongside `byte_range` so
    /// [`AdeApp::trigger_goto_definition`] can reuse it without recomputing.
    pub(in crate::code_surface) position: lsp_core::lsp_types::Position,
    pub(in crate::code_surface) status: HoverStatus,
}

/// The outcomes of one [`HoverEntry`]'s request, mirroring [`LspClientState`]'s three-state
/// shape, so `render_hover_card` can show the right state instead of a blank card while loading.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::code_surface) enum HoverStatus {
    Loading,
    /// A response arrived - `Some` for a non-empty `HoverRenderModel`, `None` for "rust-analyzer
    /// answered, nothing to show" (e.g. hovering whitespace) - never conflated with
    /// [`HoverStatus::Failed`], which means the request itself didn't complete.
    Ready(Option<hover_view::HoverRenderModel>),
    Failed(String),
}
