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

/// Which real token the pointer is currently resting on, before
/// `crate::code_surface::lsp_ui::HOVER_TRIGGER_DELAY` has elapsed and a real
/// `textDocument/hover` request has gone out for it - see
/// [`AdeApp::hover_over_token`]'s own docs for the debounce this backs.
///
/// Deliberately the exact same four fields [`HoverEntry`] carries minus its `status`: a resolved
/// [`HoverEntry`] *is* this anchor plus a real response, so [`AdeApp::hover_anchor_matches`] can
/// compare the two directly rather than through a second, independently-derived notion of "the
/// same token".
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HoverAnchor {
    pub(in crate::code_surface) path: PathBuf,
    /// 1-based, matching [`HoverEntry::line_number`]/[`AdeApp::code_cursor`].
    pub(in crate::code_surface) line_number: usize,
    pub(in crate::code_surface) byte_range: Range<usize>,
    pub(in crate::code_surface) position: lsp_core::lsp_types::Position,
}

/// The state of one in-flight or completed hover-triggered `textDocument/hover` request; see
/// [`AdeApp::hover`]'s docs for the caching discipline this backs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HoverEntry {
    /// The absolute path of the file the hovered symbol is in; `render_file_view` only shows
    /// [`Self::status`] when it matches the file currently open.
    pub(in crate::code_surface) path: PathBuf,
    /// 1-based line number (matching [`AdeApp::code_cursor`]'s convention); half of this entry's
    /// cache key along with [`Self::byte_range`].
    pub(in crate::code_surface) line_number: usize,
    /// Byte range, within the line's text, of the hovered token - the span
    /// [`crate::code_surface::file_view::render_file_view_line`] underlines with `theme::syntax::HOVER_UNDERLINE`,
    /// and the other half of the cache key.
    pub(in crate::code_surface) byte_range: Range<usize>,
    /// The LSP `Position` this request was/will be sent with, kept alongside `byte_range` so
    /// [`AdeApp::trigger_goto_definition`] can reuse it without recomputing.
    pub(in crate::code_surface) position: lsp_core::lsp_types::Position,
    pub(in crate::code_surface) status: HoverStatus,
}

impl HoverEntry {
    /// Whether this entry is worth the real `theme::syntax::HOVER_UNDERLINE` affordance at all -
    /// `false` only for a genuinely empty, already-answered [`HoverStatus::Ready(None)`]. Loading
    /// and Failed both still underline (matching the popup itself, which still shows a real
    /// "loading hover..."/"hover failed: ..." card for those - see
    /// `crate::code_surface::lsp_ui::AdeApp::render_hover_card`'s own docs), since the underline's
    /// whole job is signalling "there is something real here to look at", and both of those are.
    /// Before this, every hovered token underlined identically regardless of outcome, which read
    /// as "this is a real, hoverable symbol" even for the (very common - most identifiers rust-
    /// analyzer has nothing to say about) case where the real answer was nothing at all.
    pub(in crate::code_surface) fn worth_underlining(&self) -> bool {
        !matches!(self.status, HoverStatus::Ready(None))
    }
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

/// The outcome of the most recent (or in-flight) `wt_core::blame::blame_file` call for one
/// absolute path - see `crate::code_surface::blame_view`'s own module docs for the background/
/// caching design this backs. Kept separate from `AdeApp::blame_cache` (mirroring
/// [`FileLoadState`]/[`AdeApp::file_view_cache`]'s own split) so "still loading" and "genuinely
/// unavailable" are both real, renderable states rather than either being confused with a blank
/// cache entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlameLoadState {
    Loading,
    /// A real blame is cached in `AdeApp::blame_cache` for this path.
    Ready,
    /// Not a git repository, or the file has no history in `HEAD` (untracked, or new) - a real,
    /// expected outcome per `wt_core::blame`'s own "graceful absence" contract, never shown as
    /// an error.
    Unavailable,
    /// A genuine, unexpected failure (e.g. `git` not on `$PATH`) - still never surfaced as an
    /// error toast (GitHub issue #29's own requirement), but logged and kept distinct from
    /// [`Self::Unavailable`] so a future diagnostics surface could tell the two apart.
    Error(String),
}

/// One file's cached, real blame result plus the on-disk fingerprint it was computed from - see
/// `crate::code_surface::blame`'s own module docs ("What 'revision' means for the cache this
/// backs") for why both the commit id embedded in `blame` and this `(mtime, len)` pair together
/// form the real cache key, and `crate::code_surface::blame_view::AdeApp::spawn_blame_load` for
/// where this is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlameCacheEntry {
    pub(in crate::code_surface) mtime: Option<std::time::SystemTime>,
    pub(in crate::code_surface) len: u64,
    pub(in crate::code_surface) blame: wt_core::blame::FileBlame,
}

/// The outcome of the most recent (or in-flight) `wt_core::blame::commit_message` call for one
/// commit sha - see `AdeApp::blame_commit_messages`' own docs for why this is keyed by sha
/// (shared across every file/line that references the same commit) rather than per-path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommitMessageState {
    Loading,
    Ready(String),
    /// A genuine fetch failure - never surfaced as an error toast; the hover tooltip just falls
    /// back to the one-line blame summary it already has (see
    /// `crate::code_surface::blame::inline_blame_label`'s own `full_message` fallback).
    Failed(String),
}

#[cfg(test)]
mod hover_entry_underline_tests {
    use super::*;

    fn entry_with(status: HoverStatus) -> HoverEntry {
        HoverEntry {
            path: PathBuf::from("/scratch/sample.rs"),
            line_number: 1,
            byte_range: 0..5,
            position: lsp_core::lsp_types::Position {
                line: 0,
                character: 0,
            },
            status,
        }
    }

    /// The one real behavior change this test exists for: a genuinely empty, already-answered
    /// hover must not underline - see `HoverEntry::worth_underlining`'s own docs.
    #[test]
    fn a_genuinely_empty_answered_hover_is_not_worth_underlining() {
        assert!(!entry_with(HoverStatus::Ready(None)).worth_underlining());
    }

    /// Every other real status still is - `Loading`/`Failed` both still show a real popover of
    /// their own (see `render_hover_card`), so the underline affordance pointing at it must too.
    #[test]
    fn every_other_real_status_is_still_worth_underlining() {
        assert!(entry_with(HoverStatus::Loading).worth_underlining());
        assert!(entry_with(HoverStatus::Failed("timed out".to_string())).worth_underlining());
        assert!(
            entry_with(HoverStatus::Ready(Some(hover_view::HoverRenderModel {
                module_path: None,
                signature: "fn alpha()".to_string(),
                doc: None,
            })))
            .worth_underlining()
        );
    }
}
