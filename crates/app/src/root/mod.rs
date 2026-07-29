//! The top-level three-pane window: a left worktree sidebar, a tabbed center pane of
//! terminal sessions, and a right file tree, composed as GPUI entities.
//!
//! ## Offloading `wt-core`'s blocking calls
//!
//! `wt_core::list_worktrees` performs blocking I/O (`gix` object-database reads, and
//! sometimes spawning `git`). It's never called directly from `render` or an event handler;
//! [`AdeApp::load_worktrees`] hands it to `cx.background_executor().spawn(..)` and only
//! touches `self` again inside a `this.update(cx, ..)` callback once the background task
//! resolves. `crate::file_tree::build_file_tree`'s `std::fs::read_dir` walk follows the same
//! pattern.
//!
//! ## Selecting a worktree does not respawn a session
//!
//! [`crate::sessions::Sessions`] holds any number of independent, simultaneously-running
//! terminal sessions (a plain shell, or an agent CLI), each pinned to the worktree it was
//! started in. Selecting a worktree in the sidebar only updates [`AdeApp::selected`] (which
//! drives the file tree, and which worktree `active_session_cwd` resolves to for the *next*
//! "New Shell"/"New Claude Session" click) - it never respawns or kills anything. Spawning a
//! session is always its own explicit action (the toolbar buttons), never an implicit side
//! effect of browsing.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    actions, div, font, prelude::*, px, rems, uniform_list, App, BoxShadow, ClickEvent, Context,
    DragMoveEvent, Empty, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent,
    Pixels, ScrollStrategy, Task, UniformListScrollHandle, Window, WindowControlArea,
};
use wt_core::diff::{DiffBase, DiffFile, DiffLineKind, FileChangeStatus, WorktreeDiff};
use wt_core::merge::{ConflictHunk, ConflictSegment, ConflictedPath};

use crate::changes::{self, ChangeTag};
use crate::code_view;
use crate::diagnostics_view;
use crate::edit_buffer;
use crate::env_info;
use crate::file_tree::{self, FileTreeEntry, LangChip};
use crate::hover_view;
use crate::keymap::{self, WindowControlsStyle};
use crate::language;
use crate::layout;
use crate::merge;
use crate::palette;
use crate::process_stats;
use crate::rail::{
    self, ProjectChild, RailMode, SessionRow, StatusGroup, WorktreeEntry, WorktreeNote,
};
use crate::sessions::{Session, SessionId, SessionKind, Sessions};
use crate::settings::{self, SettingsPage};
use crate::settings_store::{self, CfgFormat, Settings};
use crate::status::{self, Status};
use crate::theme;
use crate::work_surface;
use crate::worktrees::{self, WorktreeItem};

use crate::root::code_surface::{DiffLoadState, FileLoadState, HoverEntry};
use crate::root::lsp::LspClientState;
use crate::root::resize::{PaneResizeDrag, ResizeTarget};
use crate::root::sidebar_render::RightSidebarView;
use crate::root::task_pool::TaskPool;

// Bound as GPUI actions/keybindings in `crate::default_key_bindings` (see that function's docs
// for why each literal keystroke spec was chosen, e.g. `secondary-,` over `cmd-,`).
//
// `GotoDefinition` (F12) reads `AdeApp::hover` as its target rather than tracking a separate
// "definition target" - there's no other notion of "the symbol under consideration" in this
// read-only viewer, and a hover must already have resolved before F12 can navigate. No-ops when
// nothing has been clicked yet, or the click wasn't on a `.rs` file.
//
// `JumpToSession1`..`JumpToSession8` are eight distinct zero-sized actions, one per keystroke,
// since a bound `KeyBinding` maps one literal keystroke to one action value and `actions!`-
// generated unit structs carry no data a single handler could branch on by position.
// The `Editor*` actions below (Revision R8.5a) back the File view's real text editing -
// `crate::root::editing`'s `EntityInputHandler` impl and action handlers. Bound with a
// `key_context` of `Some("file-editor")` in `crate::default_key_bindings`, scoped to only the
// editable File view container (`crate::root::code_surface::render_file_view`'s inner container,
// not the shared outer Diff/File surface `.key_context("diff")` also uses) - see that function's
// own docs for why the Diff view must never receive these bindings.
actions!(
    app,
    [
        NewSession,
        TogglePalette,
        ToggleSettings,
        GotoDefinition,
        NewTerminal,
        NewAgentPane,
        NextChangedFile,
        JumpToSession1,
        JumpToSession2,
        JumpToSession3,
        JumpToSession4,
        JumpToSession5,
        JumpToSession6,
        JumpToSession7,
        JumpToSession8,
        EditorBackspace,
        EditorDelete,
        EditorEnter,
        EditorLeft,
        EditorRight,
        EditorUp,
        EditorDown,
        EditorSelectLeft,
        EditorSelectRight,
        EditorSelectUp,
        EditorSelectDown,
        EditorHome,
        EditorEnd,
        EditorSelectAll,
        EditorCopy,
        EditorCut,
        EditorPaste,
        EditorSave,
        EditorSaveAnyway,
    ]
);

/// How often `crate::rail::compute_status_snapshot`'s background `git` status/diff refresh
/// re-runs. Coarser than `crate::terminal_pane`'s 33ms poll since this spawns real `git` child
/// processes per worktree/session path, not a cheap channel `try_recv`.
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often [`AdeApp::render_file_view`] calls `std::fs::metadata` for its freshness check -
/// throttled rather than unconditional-per-render (see
/// [`AdeApp::file_view_last_freshness_check`]).
const FILE_FRESHNESS_CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on how many changed files the diff view renders, independent of `wt_core::diff`'s own
/// `MAX_FILES` cap (300) on the loaded diff. Mirrors `file_tree::MAX_RENDERED_FILE_ENTRIES`.
const MAX_RENDERED_DIFF_FILES: usize = 40;

/// Cap on how many hunk lines a single file's diff renders, independent of `wt_core::diff`'s
/// own per-file `MAX_HUNK_LINES_PER_FILE` cap (2000) on loaded data.
const MAX_RENDERED_DIFF_LINES_PER_FILE: usize = 300;

/// The Diff view's per-hunk syntax-highlight + gutter-number cache's real shape - see
/// [`AdeApp::diff_highlight_cache`]'s own docs for what each element means and why the `DiffFile`
/// is a real identity guard, not decoration. A named `type` (rather than the bare tuple type
/// inline) so `clippy::type_complexity` doesn't fire, and so `code_surface`'s own
/// `diff_highlight_cache_for` can share the exact same shape as the field it reads.
pub(super) type DiffHighlightCache = (
    DiffFile,
    Vec<Vec<code_view::RenderedLine>>,
    Vec<Vec<(Option<usize>, Option<usize>)>>,
);

/// How often [`AdeApp::ensure_lsp_poll_task`]'s background loop checks for a newly-arrived
/// `publishDiagnostics` notification. Coarser than `crate::terminal_pane::POLL_INTERVAL` (33ms):
/// pty output is latency-sensitive, rust-analyzer's diagnostics are not.
const LSP_DIAGNOSTICS_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long [`AdeApp::request_hover`]/[`AdeApp::trigger_goto_definition`] wait for
/// rust-analyzer's response before giving up. Both run against an already-`Ready`
/// [`LspClientState`], so this budgets one query's round trip, not indexing from a cold start.
const LSP_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AdeApp {
    repo_path: PathBuf,
    worktrees: Vec<WorktreeItem>,
    worktrees_error: Option<String>,
    selected: Option<usize>,
    sessions: Sessions,
    file_tree: Vec<FileTreeEntry>,
    file_tree_root: PathBuf,
    file_tree_error: Option<String>,
    right_sidebar_view: RightSidebarView,
    diff_root: PathBuf,
    diff_state: DiffLoadState,
    /// The real `+n`/`-n` totals across every file in [`Self::diff_state`]'s currently loaded
    /// diff (`Self::render_right_sidebar_toggle`'s header totals), computed once - off the UI
    /// thread, alongside `diff_state` itself becoming `DiffLoadState::Loaded` - rather than
    /// re-folded over every one of up to 300 files' hunks on *every single render* regardless
    /// of which Zone 3 tab is even showing. `None` whenever there's no loaded diff to sum (see
    /// [`Self::current_diff`]'s docs for exactly which [`DiffLoadState`]/[`DiffBase`]
    /// combinations count).
    diff_totals: Option<(u32, u32)>,
    /// Real collapse/expand state for the file tree - a directory's path is in this set iff
    /// the user has collapsed it (see `crate::file_tree::visible_entries`, which this set
    /// feeds directly). Absence means expanded, so a freshly loaded tree starts fully open,
    /// matching the design's own default screenshots.
    collapsed_dirs: HashSet<PathBuf>,
    /// Per-file "reviewed" toggle state for the Changes list - a file's path is in this set iff
    /// its checkbox is checked. No backend "review" concept exists yet; this is purely local UI
    /// state that `Self::render_changes_header`'s progress bar and count read directly.
    reviewed_files: HashSet<PathBuf>,
    /// Ordered list of currently-open file tabs, rendered after every session's own tab by
    /// `Self::render_tab_strip`. No duplicates: opening an already-open file just activates its
    /// existing entry (`Self::push_open_file`). Removed only on explicit tab close
    /// (`Self::close_file_tab`) or leaving the owning worktree (`reset_per_worktree_ui_state`) -
    /// these are worktree-relative paths, meaningless (or collision-prone) once the worktree
    /// changes.
    open_files: Vec<PathBuf>,
    /// Which file tab (if any) the centre pane is showing instead of a session -
    /// `Some(path)` iff `path` is also in [`Self::open_files`]. Set by a Changes row
    /// (`Self::open_change_diff`), a Files-tree row (`Self::open_file_view`), or an already-open
    /// tab (`Self::activate_file_tab`); cleared by selecting a session tab or closing the active
    /// tab down to none left.
    open_change: Option<PathBuf>,
    /// Cached `DiffFile` for whichever path [`Self::open_change`] names (`None` if it has no
    /// diff, or nothing is open) - kept fresh by [`Self::refresh_open_diff_file_cache`] instead
    /// of re-cloning the whole diff (up to 2000 hunk lines) on every render.
    /// `Self::render_center_pane` moves it out (`Option::take`) rather than cloning it again
    /// before calling `Self::render_code_surface` (which needs `&mut self`) and moves it back
    /// afterward - an O(1) swap, not a second deep clone.
    open_diff_file_cache: Option<DiffFile>,
    /// File-tree path last resolved from a palette file result with no diff to open
    /// (`Self::open_palette_file_result`) - highlighted in `Self::render_file_tree_row` like a
    /// Changes row's own selection highlight.
    selected_tree_path: Option<PathBuf>,
    /// Surface C's `Diff | File` toggle for whichever file [`Self::open_change`] names - set to
    /// `Diff` by [`Self::open_change_diff`] and `File` by [`Self::open_file_view`], read by
    /// [`Self::render_code_surface`] alongside a "does this file even have a diff" check (a
    /// diff-less file always renders as `File` regardless of this field).
    code_view: code_view::CodeView,
    /// Surface C's Diff/File focus target, `track_focus`'d by
    /// [`Self::render_code_surface`]'s outer container - see [`OverlayFocus`]/[`restore_focus`]
    /// for the dangling-focus invariant this and [`Self::code_focus`] exist to satisfy.
    code_focus_handle: FocusHandle,
    /// Pre-open focus target for [`Self::code_focus_handle`] - see [`OverlayFocus`].
    code_focus: OverlayFocus,
    /// The File view's `uniform_list` scroll handle (`gpui::UniformListScrollHandle`, matching
    /// `vendor/zed/crates/git_ui/src/git_panel.rs`'s `commit_history_scroll_handle` use of the
    /// same type) - driven by go-to-definition landing on a distant [`Self::code_cursor`] line,
    /// never on an ordinary click or fresh file open (no reason to fight the user's own scroll
    /// position).
    file_view_scroll_handle: UniformListScrollHandle,
    /// Cached parse/highlight of whichever file [`Self::render_file_view`] last loaded
    /// (`code_view::load_file`/`highlight_rust`) - reused unless `code_view::cache_is_fresh`
    /// says otherwise, always written from [`Self::spawn_file_load`]'s background task, never
    /// synchronously during `render()`.
    file_view_cache: Option<code_view::ParsedFile>,
    /// Cached per-hunk syntax highlighting (and per-hunk gutter line numbers, so
    /// [`Self::render_diff_file_detail`]'s render loop never recomputes
    /// [`changes::hunk_line_numbers`] itself) for whichever `DiffFile` this cache was built from,
    /// with both outer `Vec`s index-aligned with `file.hunks`, inner with that hunk's `lines`.
    /// Freshness is a cheap `DiffFile` equality check (see
    /// [`Self::ensure_diff_highlight_cache`]), mirroring [`Self::file_view_cache`]'s "recompute
    /// only when stale" discipline for the Diff view. The cached `DiffFile` (first tuple field)
    /// is a real identity guard, not decoration: [`Self::render_diff_file_detail`] must filter on
    /// it equalling the file it's actually rendering before reading either `Vec` positionally,
    /// per that method's own docs, for the real bug (rendering one file's cached source lines
    /// under another file's diff signs/gutter numbers) skipping this check would allow. Cleared
    /// on a worktree switch alongside [`Self::open_diff_file_cache`] (see
    /// [`Self::select_worktree`]) so it never retains a full file's highlighting from the
    /// worktree just left.
    diff_highlight_cache: Option<DiffHighlightCache>,
    /// Path and time [`Self::render_file_view`] last called `std::fs::metadata` for its
    /// freshness check, throttling that syscall to at most once per
    /// [`FILE_FRESHNESS_CHECK_INTERVAL`] instead of every render. `None` until the first check;
    /// `Self::select_worktree` doesn't need to reset this since a worktree switch always changes
    /// `file_tree_root`, forcing a path mismatch and thus a fresh check anyway.
    file_view_last_freshness_check: Option<(PathBuf, Instant)>,
    /// See [`FileLoadState`]'s own docs.
    file_load_state: FileLoadState,
    /// Changed-line set (`code_view::changed_line_set`) for whichever `DiffFile`
    /// [`Self::open_diff_file_cache`] holds - recomputed only by
    /// [`Self::refresh_open_diff_file_cache`], never per render.
    file_view_changed_lines: HashSet<usize>,
    /// The File view's "last click" cursor line (1-indexed), set by
    /// [`Self::render_file_view_line`]'s click handler and reset to `1` on a fresh file load.
    /// No column tracking: per-character hit-testing against a monospace run wasn't implemented
    /// this phase, so no column is shown at all rather than a fabricated `col 1`.
    code_cursor: Option<usize>,
    /// Surface C's current editor zoom for whichever file [`Self::open_change`] names - a
    /// percentage of `Settings.appearance.editor_font_size`'s 100% baseline, clamped to
    /// `code_surface::ZOOM_MIN_PERCENT..=ZOOM_MAX_PERCENT` by
    /// [`code_surface::clamp_zoom_percent`], written only through
    /// [`Self::zoom_in`]/[`Self::zoom_out`]/[`Self::reset_zoom`].
    code_zoom_percent: u16,
    /// Each open file tab's independently-remembered zoom - only read/written while
    /// `Settings.appearance.per_tab_zoom` is on; otherwise every tab shares
    /// [`Self::code_zoom_percent`]. Keyed like [`Self::open_files`], so it gets the same
    /// per-worktree reset in `reset_per_worktree_ui_state`.
    file_zoom_percent: HashMap<PathBuf, u16>,
    /// Real, live per-tab text-editing state for the File view (Revision R8.5a) - keyed the same
    /// way as [`Self::open_files`]/[`Self::file_zoom_percent`] (a worktree-relative path), so
    /// switching between open file tabs never loses unsaved edits in a background tab. Created
    /// lazily the first time a file is opened in File view (see
    /// [`crate::root::code_surface::AdeApp::render_file_view`]), seeded from the exact same
    /// background read [`Self::spawn_file_load`] already performs. [`Self::file_view_cache`]
    /// stays the freshness-check/diagnostics/hover source of truth (the last-*saved* snapshot,
    /// per this phase's own scope); this map is what's actually on screen and what an explicit
    /// save writes. Deliberately **not** removed on an ordinary tab close
    /// ([`crate::root::code_surface::AdeApp::close_file_tab`]) - dropping unsaved edits just
    /// because a tab was closed (with no "save before closing?" prompt - out of scope this
    /// phase) would be a real, silent data-loss risk; reopening the same file later restores the
    /// exact in-memory buffer. Reset alongside `open_files` in `reset_per_worktree_ui_state` so a
    /// worktree switch doesn't leak another worktree's buffers, matching the same
    /// per-worktree-reset convention `open_files`/`file_zoom_percent` already follow.
    edit_buffers: HashMap<PathBuf, edit_buffer::EditBuffer>,
    /// Every visible File-view row's real painted bounds and shaped line, captured by
    /// `crate::root::editing`'s per-row `gpui::canvas` paint callback each render - read back by
    /// a row's own click handler to hit-test a click into a real byte offset
    /// (`gpui::LineLayout::closest_index_for_x`) for real click-to-place-cursor. Keyed by 1-based
    /// line number (matching [`Self::code_cursor`]'s convention). Transient/best-effort: entries
    /// for rows no longer visible are simply never refreshed again (harmless - a stale entry is
    /// only ever read for a row-click hit test, and a scrolled-away row can't be clicked), so this
    /// is cleared wholesale only on a worktree switch, not pruned every frame.
    file_view_row_layout: HashMap<usize, (gpui::Bounds<Pixels>, gpui::ShapedLine)>,
    /// The real shaped line, bounds, and 0-indexed buffer line that painted the *caret's own* row
    /// most recently - `crate::root::editing`'s `EntityInputHandler::bounds_for_range`/
    /// `character_index_for_point` read these three together (never `file_view_row_layout`, which
    /// only serves click hit-testing) and honestly return `None` when the caret's row wasn't
    /// actually painted last frame (e.g. scrolled out of view) - the same real, structural
    /// "degrade honestly when the query can't be answered" behavior
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own `TextInput::last_layout`/`last_bounds`
    /// have, scoped here to whichever path/line they're actually for so a stale entry from a
    /// different file/line can never be misapplied.
    file_view_last_layout: Option<gpui::ShapedLine>,
    file_view_last_bounds: Option<gpui::Bounds<Pixels>>,
    /// The path and 0-indexed buffer line [`Self::file_view_last_layout`]/
    /// [`Self::file_view_last_bounds`] are for - `None` until the first paint of a caret row.
    file_view_last_layout_for: Option<(PathBuf, usize)>,
    /// Every in-flight debounced real re-highlight (`code_view`'s real `tree-sitter` parse) for a
    /// dirty [`Self::edit_buffers`] entry, keyed by the same relative path - see
    /// [`edit_buffer`]'s own "Re-highlighting cost" docs for why this is debounced rather than
    /// run inline on every keystroke. A single slot per path (not a `TaskPool`): only the most
    /// recent keystroke's debounce for a given file should ever fire, so a fresh keystroke
    /// re-arming the same path's entry correctly cancels (drops) whatever shorter-lived timer was
    /// still waiting - no risk of an out-of-order *write*, unlike
    /// [`Self::_file_save_tasks`]'s real disk writes, since this only ever reads
    /// `edit_buffers`/writes back into it, gated by [`crate::edit_buffer::EditBuffer::
    /// apply_highlight`]'s own real content-snapshot equality check.
    _rehighlight_tasks: HashMap<PathBuf, Task<()>>,
    /// Every in-flight explicit `crate::root::editing::AdeApp::save_active_file` background
    /// write, one slot per path - see [`Self::file_save_pending`]/[`Self::file_save_running`]'s
    /// own docs for the serial-writer-loop discipline (mirroring
    /// [`Self::_settings_save_task`]'s own, per-path rather than global) that keeps two saves of
    /// the *same* file from ever racing on disk, the exact class of bug Revision R5.5 fixed once
    /// for settings.
    _file_save_tasks: HashMap<PathBuf, Task<()>>,
    /// Paths with a save requested while that same path's serial writer loop
    /// ([`Self::_file_save_tasks`]) was already running - picked up by the loop's own next
    /// iteration rather than racing a second, independent `std::fs::write` against the same file.
    file_save_pending: HashSet<PathBuf>,
    /// Paths whose serial writer loop is currently alive - guards against spawning a second loop
    /// for a path that already has one draining [`Self::file_save_pending`].
    file_save_running: HashSet<PathBuf>,
    /// The most recent explicit save's real failure, if any (e.g. a permission error, disk full) -
    /// surfaced honestly near the File view's tab strip rather than silently dropped. Also holds
    /// the real external-change-conflict message (see [`Self::file_external_conflict`]) sharing
    /// this same small error-surfacing convention. Cleared only by a subsequent successful save
    /// (ordinary or the real `EditorSaveAnyway` override) of the same path - **not** by closing
    /// that tab: [`Self::edit_buffers`] is itself deliberately preserved across a tab close (see
    /// that field's own docs, "not removed on an ordinary tab close") so real unsaved edits
    /// survive, and a real unresolved save error/conflict for that same still-dirty buffer is
    /// exactly as real after the tab closes as before - silently dropping the warning just
    /// because the tab isn't visible would be the same "looks resolved, isn't" risk this whole
    /// mechanism exists to avoid.
    file_save_error: Option<(PathBuf, String)>,
    /// Paths currently flagged with a real, detected conflict: an unsaved (dirty) edit buffer
    /// whose underlying file has genuinely changed on disk since it was loaded/last saved (some
    /// other process - git, the agent CLI in a terminal tab, an external editor - touched it).
    /// Set by [`crate::root::code_surface::AdeApp::render_file_view`]'s existing freshness check
    /// when it fires while the buffer is dirty; cleared once the check no longer finds a mismatch.
    /// [`crate::root::editing::AdeApp::save_active_file`] refuses to save (with its own,
    /// authoritative fresh `std::fs::metadata` check, not just this render-throttled flag) while
    /// a path is in this set - see that method's own docs for why silently overwriting the
    /// external change, or silently discarding the user's own unsaved edits, are both wrong.
    file_external_conflict: HashSet<PathBuf>,
    /// Whether the command palette (⌘K) overlay is open.
    palette_open: bool,
    /// The palette's active scope (`All`/`Commands`/`Files`).
    palette_scope: palette::PaletteScope,
    /// The palette's currently typed query - the same minimal hand-rolled append/backspace text
    /// field as [`Self::filter_query`] (see [`Self::handle_filter_key_down`]'s docs for why, over
    /// `vendor/zed/crates/gpui/examples/input.rs`'s full `EntityInputHandler`).
    palette_query: String,
    /// The palette's currently highlighted result row - an index into
    /// `Self::build_palette_groups`' flattened (`crate::palette::flatten`) row order, moved by
    /// arrow keys and run by Enter.
    palette_selected: usize,
    palette_focus_handle: FocusHandle,
    /// Pre-open focus target and active session for [`Self::palette_focus_handle`] - see
    /// [`OverlayFocus`]/[`restore_focus`].
    palette_focus: OverlayFocus,
    /// The palette's file-candidate list (`crate::palette::FileCandidate`, one per non-directory
    /// [`Self::file_tree`] entry, up to `file_tree::MAX_ENTRIES` = 5000) - built once by
    /// [`Self::rebuild_palette_file_candidates`] when `file_tree`/the diff reload, not rebuilt on
    /// every `Self::build_palette_groups` call (which runs on every render while the palette is
    /// open, up to ~30x/sec during a streaming session). Session/command candidates aren't
    /// cached the same way: they're few, and a session's status dot is genuinely live per-render
    /// data with no stable invalidation point.
    palette_file_candidates: Vec<palette::FileCandidate>,
    /// The session rail's user-adjustable width (240-340px), dragged via the resize handle on
    /// the rail's right edge (see [`Self::apply_pane_resize`]/`crate::layout::rail_width_for_cursor`).
    rail_width: Pixels,
    /// The files/changes panel's user-adjustable width - see [`Self::rail_width`]'s docs,
    /// mirrored on the panel's left edge (`crate::layout::panel_width_for_cursor`).
    panel_width: Pixels,
    /// The window body's current paint bounds - captured every render by a `gpui::canvas` child
    /// (see [`Self::render`]'s body child list) and read by [`Self::apply_pane_resize`] to turn
    /// a drag's cursor position into a pane width, the same pattern
    /// `vendor/zed/crates/workspace/src/workspace.rs`'s own `bounds` field uses for its dock
    /// resize. `Bounds::default()` until the first paint; harmless since nothing reads it before
    /// a resize handle can be dragged.
    body_bounds: gpui::Bounds<Pixels>,
    /// Armed by a left mouse-down on the title bar's drag area, consumed by the next mouse-move
    /// to call `Window::start_window_move` - see [`Self::render_title_bar`]'s docs for why this
    /// two-step dance (verified against `vendor/zed/crates/platform_title_bar/src/
    /// platform_title_bar.rs`'s same pattern) is needed instead of starting the move on
    /// mouse-down directly.
    title_bar_move_armed: bool,
    /// The session rail's grouping mode (`by urgency` / `by project`). See
    /// [`crate::rail::RailMode`].
    rail_mode: RailMode,
    /// The rail's filter query - filters the rendered session/worktree rows in both grouping
    /// modes (see `crate::rail::filter_sessions`/`filter_project_children`).
    filter_query: String,
    filter_focus_handle: FocusHandle,
    /// Real `+N -M`/has-changes totals per worktree or session cwd, refreshed by the
    /// periodic background task started in `Self::new` - see `crate::rail::
    /// compute_status_snapshot`'s docs. Read (never written outside that task's completion
    /// callback) by `Self::build_session_rows` each render.
    diff_cache: HashMap<PathBuf, rail::DiffSummary>,
    /// Real clean/merged notes per worktree path, from the same periodic refresh as
    /// [`Self::diff_cache`] - powers "by project" mode's session-less worktree rows and the
    /// rail footer's `prune` action.
    worktree_notes: HashMap<PathBuf, rail::WorktreeNote>,
    /// Real `wt_core::diff::AheadBehind` counts per worktree/session cwd, from the same
    /// periodic refresh as [`Self::diff_cache`] - the status bar's `↑2 ↓0` indicator for the
    /// active session's worktree.
    ahead_behind_cache: HashMap<PathBuf, wt_core::diff::AheadBehind>,
    /// Real, live per-pid CPU%/memory samples for every currently open session's process
    /// (`crate::process_stats`), refreshed by the same periodic background task as
    /// [`Self::diff_cache`] - see `Self::start_status_polling`'s docs for why this rides the
    /// same timer rather than a second, independent polling loop. Keyed by OS pid; an entry is
    /// absent for a pid not yet sampled (or already exited).
    process_stats: HashMap<u32, process_stats::ProcessSample>,
    /// Real, bounded disk-usage total across every listed worktree (see
    /// `crate::rail::disk_usage_bytes`'s docs for the real `std::fs` walk and its cap),
    /// recomputed whenever the worktree list reloads. `None` while the very first
    /// computation is still in flight.
    disk_usage: Option<(u64, bool)>,
    /// Per-worktree half of the same computation [`Self::disk_usage`] sums
    /// (`crate::rail::disk_usage_bytes(path)`, see [`Self::load_disk_usage`]) - kept as its own
    /// field because the Settings › Worktrees page needs a per-row size, not just the rail
    /// footer's aggregate.
    worktree_disk_usage: HashMap<PathBuf, (u64, bool)>,
    /// Feedback from the most recent `prune` click (how many worktrees were removed, or an
    /// error), shown in the rail footer until the next prune attempt or worktree reload.
    prune_status: Option<String>,
    /// `true` after one click on the footer `prune` button, cleared after the confirming click
    /// or by any other rail interaction in the meantime - see [`Self::request_prune`]'s docs for
    /// why prune is a two-click confirmation.
    prune_confirm_armed: bool,
    /// `true` for the duration of an in-flight [`Self::execute_prune`] batch - guards against a
    /// second confirming click spawning a second, racing batch that would overwrite
    /// [`Self::_prune_task`] and drop (cancel) the first mid-flight. Set synchronously before
    /// spawning, reset in that same task's completion handler.
    prune_in_flight: bool,
    /// Whether the Settings surface is currently replacing the three-zone body - see
    /// [`Self::open_settings`]/[`Self::close_settings`], which use the same
    /// capture-and-restore shape as [`Self::palette_open`].
    settings_open: bool,
    /// Which Settings nav page is selected - persists across opens/closes (unlike the palette's
    /// query/scope, which resets every time).
    settings_page: settings::SettingsPage,
    settings_focus_handle: FocusHandle,
    /// Pre-open focus target for [`Self::settings_focus_handle`] - see [`OverlayFocus`].
    settings_focus: OverlayFocus,
    /// The Settings › Agents page's rows (`crate::settings::detect_agent_rows`, via
    /// `pty_core::resolve_on_path`), computed off the foreground thread and cached here (see
    /// [`Self::load_agent_rows`]) rather than recomputed inline - a `$PATH` search for a
    /// not-found binary measures ~30ms, which would cap the whole Settings surface's frame rate
    /// if run inline. `Vec::new()` until the first load completes.
    agent_rows: Vec<settings::AgentRow>,
    /// The context bar's `Merge` action and Surface D's conflict-resolution flow - see
    /// [`crate::merge::MergeFlow`]'s docs. `None` when no session has an in-flight merge or
    /// unresolved conflict.
    merge_flow: Option<merge::MergeFlow>,
    /// `true` for the duration of an in-flight `Complete merge`/`Abort merge` git operation -
    /// guards against a fast Abort-after-Complete double-click letting `git merge --abort` race
    /// an in-flight `git commit` and discard already-resolved conflict work.
    merge_op_in_flight: bool,
    /// Cached per-side syntax highlighting for whichever conflict hunk is currently active in
    /// [`Self::merge_flow`] - recomputed only at the real points that can change (`start_merge`
    /// finding a `Conflicted` state, `resolve_active_hunk` advancing to the next hunk), never
    /// from `render()`. Keyed on `(relative_path, ConflictHunk)`, not the hunk alone: two
    /// different conflicted files can have byte-identical hunk content (same lines/labels/start
    /// lines) but different extensions, which would otherwise incorrectly reuse one file's
    /// highlighting for the other - `ConflictHunk` already derives `PartialEq`, `PathBuf`'s own
    /// is a plain path compare.
    merge_highlight_cache: Option<(
        PathBuf,
        ConflictHunk,
        Vec<code_view::RenderedLine>,
        Vec<code_view::RenderedLine>,
    )>,
    _load_worktrees_task: Option<Task<()>>,
    _load_file_tree_task: Option<Task<()>>,
    _load_diff_task: Option<Task<()>>,
    /// The in-flight `code_view::load_file` task for whichever path [`FileLoadState::Loading`]
    /// names - dropping it (a fresh assignment, or `Self::select_worktree`'s reset) cancels that
    /// load immediately, per GPUI's `Task`-drop-cancels semantics.
    _file_load_task: Option<Task<()>>,
    _status_poll_task: Option<Task<()>>,
    _disk_usage_task: Option<Task<()>>,
    _prune_task: Option<Task<()>>,
    _agent_rows_task: Option<Task<()>>,
    _merge_task: Option<Task<()>>,
    /// `Self::clear_merge_flow_for_closed_session`'s best-effort abort - kept separate from
    /// [`Self::_merge_task`] so a cleanup-triggered abort can never overwrite (and thus cancel)
    /// an in-flight `complete_merge_flow`/`abort_merge_flow` commit, which would strand
    /// [`Self::merge_op_in_flight`] at `true` and let `git merge --abort` race an in-flight
    /// `git commit`.
    _merge_cleanup_task: Option<Task<()>>,
    /// Every in-flight [`Self::resolve_active_hunk`] background write
    /// (`wt_core::merge::write_resolved_file`) - a [`TaskPool`], not a single slot, since
    /// resolving one file's hunk while a different file's write is still in flight must not
    /// cancel that earlier write (dropping a `Task` cancels it immediately) and leave real
    /// conflict markers on disk while the in-memory model reports it resolved.
    _merge_write_tasks: TaskPool,
    /// A `lsp_core::LspClient` per `(repository root, server binary)` pair - widened from a
    /// bare `PathBuf` key (Revision R8) so more than one language's server can run
    /// simultaneously under the same repo root without colliding in this map: opening a `.rs`
    /// and a `.ts` file under the same worktree spawns two independent entries here, keyed by
    /// `(file_tree_root, "rust-analyzer")` and `(file_tree_root, "typescript-language-server")`
    /// respectively - see `crate::language::lsp_binary_for_extension` for where the second half
    /// of the key comes from. Extensions that share one server (`.ts`/`.tsx`/`.js`/`.jsx` all
    /// route to `typescript-language-server`) correctly reuse the same entry, since the key is
    /// the shared *binary*, not a per-extension language id. Spawned lazily (see
    /// [`Self::ensure_lsp_client`]) and reused for every subsequent file of that language under
    /// that root. See [`LspClientState`]'s own docs for the states a client can be in.
    ///
    /// [`Self::evict_stale_lsp_clients`] (called on every worktree switch) tears down every
    /// entry whose root isn't the newly active one - a deliberate "kill the non-active one"
    /// choice over a small bounded LRU, since each language server instance costs real GB
    /// against this repo's own workspace and worktree switches are infrequent enough that
    /// keeping more than one warm isn't worth the memory. This still applies per-root, not
    /// per-(root, binary): switching worktrees evicts *every* language's client for the old
    /// root, not just one - see `lsp::lsp_client_eviction_tests` for the regression test.
    lsp_clients: HashMap<(PathBuf, &'static str), LspClientState>,
    /// Absolute paths that have already had `textDocument/didOpen` sent for their owning
    /// [`Self::lsp_clients`] entry - checked by [`Self::render_file_view`] so a re-render never
    /// re-sends `didOpen` with a stale version. Never removed on file close: this viewer
    /// deliberately doesn't send a matching `didClose` (see [`Self::dispatch_did_open`]).
    lsp_opened_files: HashSet<PathBuf>,
    /// Per-line diagnostic index (`crate::diagnostics_view::index_diagnostics_by_line`) for
    /// whichever Rust file [`Self::render_file_view`] last rendered - recomputed at the start of
    /// every render for a Rust file, cleared for a non-Rust file so diagnostics can't bleed
    /// across files.
    file_view_diagnostics: HashMap<usize, Vec<diagnostics_view::LineDiagnostic>>,
    /// The real error-severity diagnostic count for whichever file [`Self::render_file_view`]
    /// most recently rendered a `rust-analyzer` status for - exactly the same
    /// `lsp::LspFileStatus::Analyzed { errors, .. }` value `code_surface::render_file_status_bar`
    /// itself displays for that same file, in the same frame (set right alongside
    /// [`Self::file_view_diagnostics`], from the same `lsp_status` computation). `None` whenever
    /// that render didn't produce a real `Analyzed` status (non-Rust file, or the LSP client is
    /// still spawning/indexing/failed) - not a fabricated `Some(0)`.
    /// [`Self::status_bar_error_count`] reads this rather than re-deriving a count from
    /// [`Self::file_view_diagnostics`]'s own per-line index (whose per-line shape would
    /// over-count any diagnostic spanning multiple lines), so the two real error counts shown in
    /// the same frame - this one and the File view's own footer - can never disagree.
    file_view_error_count: Option<usize>,
    /// Every in-flight `lsp_core::LspClient::spawn`/`did_open` background task - a [`TaskPool`]
    /// for the same "independent operations" reason as [`Self::_merge_write_tasks`].
    _lsp_tasks: TaskPool,
    /// The single, long-lived background poll loop watching every ready [`Self::lsp_clients`]
    /// entry's wake channel and calling `cx.notify()` on a new `publishDiagnostics`. Started
    /// lazily (see [`Self::ensure_lsp_poll_task`]), then deliberately never reset to `None` -
    /// one poll loop serves however many clients exist.
    _lsp_poll_task: Option<Task<()>>,
    /// Surface C's hover-state cache - the outcome of the most recent click-triggered
    /// `textDocument/hover` request (see [`Self::request_hover`]), `None` before the first click
    /// or after switching files. Also doubles as [`Self::trigger_goto_definition`]'s target:
    /// there's no separately-tracked "symbol under consideration" in this read-only viewer.
    hover: Option<HoverEntry>,
    /// The single in-flight [`Self::request_hover`] background task, if any - a single slot
    /// (not a [`TaskPool`]) because hover requests are never independent: [`Self::hover`] shows
    /// only one entry at a time, so a new click always supersedes an in-flight one. Assigning a
    /// fresh task here drops the previous one immediately, closing the bug where rapid clicking
    /// during rust-analyzer's initial indexing (each hover request can block a worker thread for
    /// up to [`LSP_QUERY_TIMEOUT`]) let unbounded concurrent requests pin the shared executor.
    _hover_request_task: Option<Task<()>>,
    /// Every in-flight [`Self::trigger_goto_definition`] background task - a [`TaskPool`], unlike
    /// [`Self::_hover_request_task`]'s single slot, since F12 has no `still_current`
    /// short-circuit tying it to one live UI slot the way hover does.
    _goto_definition_tasks: TaskPool,
    /// One-shot "the next completed load of this exact file should land the cursor on this
    /// line, not line 1" instruction for [`Self::spawn_file_load`]'s completion handler, set by
    /// [`Self::navigate_to_definition`] when a go-to-definition result names a file that isn't
    /// already open. Keyed by the target path (not just a line number) so an unrelated file's
    /// completed load can never misapply a stale entry meant for a different, still-loading
    /// file. Consumed via `Option::take` (only when the path matches) by whichever of
    /// [`Self::render_file_view`] or `spawn_file_load`'s completion handler applies it first;
    /// explicitly cleared by [`Self::open_file_view`] on every fresh open and by a failed load.
    pending_cursor_line: Option<(PathBuf, usize)>,
    /// The config-file-backed settings struct - loaded once from `~/.config/jerry/
    /// settings.toml` at startup ([`Self::new`], via `Settings::load_or_init`) and re-saved
    /// ([`Self::persist_settings`]) on every change from the settings pages or the palette's
    /// `Window controls: …` entries. See `crate::settings_store`'s module docs for which fields
    /// are persisted-only vs. also applied.
    settings: Settings,
    /// The resolved path [`Self::persist_settings`] writes to - `Some(~/.config/jerry/
    /// settings.toml)` in production, `None` for every GPUI test (`Self::new_with_settings`),
    /// which makes a save a genuine no-op rather than a special-cased test skip.
    settings_path: Option<PathBuf>,
    /// The single in-flight [`Self::persist_settings`] serial-writer-loop task. Settings saves
    /// are never independent of each other (there is only one `settings.toml`), so this is a
    /// single slot rather than a [`TaskPool`]: a second edit while a save is running is picked
    /// up by the still-running loop (see [`Self::settings_save_pending`]/
    /// [`Self::settings_save_running`]), not raced against it with a second write. The loop
    /// always fully awaits one `save_at` call before checking for a newer edit, so two physical
    /// writes to the same path can never be concurrent - closing a real out-of-order-write bug a
    /// simpler "drop the previous task" approach could not, since dropping a `Task` cannot stop
    /// a disk write that has already started on a worker thread.
    _settings_save_task: Option<Task<()>>,
    /// `true` whenever there's a settings edit newer than the last write the serial writer loop
    /// started - a single flag, not a queue, since only the latest value at write time ever
    /// matters. Cleared only by the loop itself, in the same step that reads [`Self::settings`]
    /// fresh to write it.
    settings_save_pending: bool,
    /// `true` for as long as the serial writer loop ([`Self::_settings_save_task`]) is alive -
    /// guards [`Self::persist_settings`] against spawning a second loop while one is already
    /// draining [`Self::settings_save_pending`].
    settings_save_running: bool,
    /// Test-only seam: an artificial delay the serial writer loop awaits (via the GPUI test
    /// clock) before each `Settings::save_at` call, letting a test deterministically hold one
    /// edit's write pending while a later edit queues behind it. `#[cfg(test)]`-gated end to
    /// end, so no test-only state exists in a production build. Set via
    /// [`Self::set_settings_save_test_delay`].
    #[cfg(test)]
    settings_save_test_delay: Option<Duration>,
    /// The config banner's `TOML | JSON` display segment - not itself a [`Settings`] field; see
    /// `crate::settings_store`'s "TOML is the real file; JSON is a read-only alternate view"
    /// docs.
    settings_cfg_format: CfgFormat,
    /// The Settings › Language servers page's rows (`crate::settings::detect_lsp_rows`), cached
    /// the same way [`Self::agent_rows`] is.
    lsp_rows: Vec<settings::LspRow>,
    _lsp_rows_task: Option<Task<()>>,
    /// The Keybindings settings page's filter query - same minimal append/backspace shape as
    /// [`Self::filter_query`].
    settings_keymap_filter: String,
    settings_keymap_filter_focus_handle: FocusHandle,
    /// Whether the tab strip's `+` menu popover is open - see [`Self::render_plus_menu`].
    /// Closed by its own scrim click, by picking a row, and defensively by
    /// [`Self::open_palette`]/[`Self::open_settings`] (it's rendered as an unconditional sibling
    /// of both, so it would otherwise paint over a surface it no longer makes sense above).
    plus_menu_open: bool,
    /// The tab strip's `+` button's painted bounds, captured every render (same `gpui::canvas`
    /// pattern as [`Self::body_bounds`]). [`Self::render_plus_menu`] positions the popover
    /// directly off this rather than a second, independently-computed offset that could drift
    /// once the rail's adjustable width shifts the button. `Bounds::default()` until first paint.
    plus_button_bounds: gpui::Bounds<Pixels>,
    /// Every in-flight [`Self::new_agent_pane`] background `$PATH` detection - a [`TaskPool`]
    /// rather than a single slot, so two rapid "New agent pane" clicks each produce their own
    /// session instead of the second cancelling the first's still-in-flight search.
    _new_agent_pane_task: TaskPool,
}

impl AdeApp {
    /// Single source of truth for which platform's title-bar variant/keycap glyphs render -
    /// [`Self::settings`]`.window.controls` is the persisted backing; the General settings page
    /// and the palette's `Window controls: …` entries both read/write it through this accessor
    /// and [`Self::set_window_controls_style`], never a second copy.
    pub(super) fn window_controls_style(&self) -> WindowControlsStyle {
        self.settings.window.controls
    }

    /// Shared entry point for `Settings.appearance.interface_scale_percent` text scaling -
    /// scales only the text size passed to `.text_size(...)`, nothing else (padding/spacing/
    /// icon/fixed-chrome dimensions are out of scope).
    pub(super) fn ui_text_size(&self, base_px: f32) -> Pixels {
        theme::ui_scale::scaled_px(base_px, self.settings.appearance.interface_scale_percent)
    }

    /// Sets [`Self::window_controls_style`] and persists it. The one write path both the
    /// General settings page and the palette's `Window controls: …` entries call, so they can
    /// never disagree about which override is active.
    pub(super) fn set_window_controls_style(
        &mut self,
        style: WindowControlsStyle,
        cx: &mut Context<Self>,
    ) {
        self.settings.window.controls = style;
        self.persist_settings(cx);
        cx.notify();
    }

    /// Queues a background-executor save of the current [`Self::settings`] to
    /// [`Self::settings_path`] (`Settings::save_at`), called from every settings mutation. A
    /// `None` path (every GPUI test) makes this a no-op; a save failure is logged, not surfaced.
    ///
    /// Marks [`Self::settings_save_pending`] and starts the serial writer loop
    /// ([`Self::_settings_save_task`]) if it isn't already running - see that field's docs for
    /// why writes are serialized rather than raced.
    pub(super) fn persist_settings(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        self.settings_save_pending = true;
        if self.settings_save_running {
            // The loop below is already alive and always re-checks `settings_save_pending`
            // before writing or stopping (see its own body) - it will pick this edit up on
            // its own next iteration. Spawning a second loop here is exactly the shape that
            // would let two real `save_at` calls overlap again.
            return;
        }
        self.settings_save_running = true;
        let task = cx.spawn(async move |this, cx| {
            loop {
                // Atomically (within this one synchronous foreground step) either take the
                // pending edit to write, or - if there is none - clear `settings_save_running`
                // in the very same step that decides to stop, so a `persist_settings` call
                // can never land in the gap between "loop decided to stop" and "loop actually
                // stopped" and be silently missed.
                let step = this.update(cx, |this, _cx| {
                    if this.settings_save_pending {
                        this.settings_save_pending = false;
                        Some(this.settings.clone())
                    } else {
                        this.settings_save_running = false;
                        None
                    }
                });
                let Ok(Some(settings)) = step else {
                    // Either the entity is gone (window closed while this loop was still
                    // running) or there is genuinely nothing left pending - either way, stop.
                    break;
                };
                // Test-only seam - see [`Self::settings_save_test_delay`]'s own docs. This
                // whole block (field read included) does not exist in a production build.
                #[cfg(test)]
                {
                    let delay = this.update(cx, |this, _cx| this.settings_save_test_delay);
                    if let Ok(Some(delay)) = delay {
                        cx.background_executor().timer(delay).await;
                    }
                }
                let result = cx
                    .background_executor()
                    .spawn({
                        let path = path.clone();
                        async move { settings.save_at(&path) }
                    })
                    .await;
                if let Err(err) = result {
                    log::warn!("failed to save {}: {err}", path.display());
                }
            }
        });
        self._settings_save_task = Some(task);
    }

    /// Test-only seam - see [`Self::settings_save_test_delay`]'s own docs.
    #[cfg(test)]
    pub(crate) fn set_settings_save_test_delay(&mut self, delay: Option<Duration>) {
        self.settings_save_test_delay = delay;
    }
}

impl Render for AdeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::surface::WINDOW)
            .font(font(theme::font::SANS))
            .on_action(cx.listener(Self::handle_new_session_action))
            .on_action(cx.listener(Self::handle_toggle_palette_action))
            .on_action(cx.listener(Self::handle_toggle_settings_action))
            .on_action(cx.listener(Self::handle_goto_definition_action))
            .on_action(cx.listener(Self::handle_new_terminal_action))
            .on_action(cx.listener(Self::handle_new_agent_pane_action))
            .on_action(cx.listener(Self::handle_next_changed_file_action))
            .on_action(cx.listener(Self::handle_jump_to_session_1_action))
            .on_action(cx.listener(Self::handle_jump_to_session_2_action))
            .on_action(cx.listener(Self::handle_jump_to_session_3_action))
            .on_action(cx.listener(Self::handle_jump_to_session_4_action))
            .on_action(cx.listener(Self::handle_jump_to_session_5_action))
            .on_action(cx.listener(Self::handle_jump_to_session_6_action))
            .on_action(cx.listener(Self::handle_jump_to_session_7_action))
            .on_action(cx.listener(Self::handle_jump_to_session_8_action))
            .child(self.render_title_bar(cx))
            // The Settings surface (`design_handoff_jerry_ade/README.md`: "a separate surface,
            // not a modal: it replaces the three zones while the title bar and status bar
            // stay") swaps out only this one child - the title bar above and the status bar
            // below are unconditional siblings, rendered every frame regardless of
            // `settings_open`.
            .child(if self.settings_open {
                self.render_settings(cx).into_any_element()
            } else {
                self.render_workspace_body(cx).into_any_element()
            })
            .child(self.render_status_bar(cx))
            .when(self.plus_menu_open, |el| {
                el.child(self.render_plus_menu(cx))
            })
            .when(self.palette_open, |el| el.child(self.render_palette(cx)))
    }
}

impl AdeApp {
    /// The three-zone workspace body (session rail, centre pane, files/changes panel) - pulled
    /// out of [`Render::render`] so it and [`Self::render_settings`] can both be
    /// [`gpui::AnyElement`]-branched as a single child, the same pattern
    /// `vendor/zed/crates/gpui/src/util.rs`'s `Styled::when_else` uses.
    fn render_workspace_body(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("body")
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            // `on_drag_move` fires for every mouse-move while a `PaneResizeDrag` is active
            // anywhere in the window, not just over the resize handle that started it -
            // registered here, on the body containing both handles, so a fast drag still
            // tracks after the cursor leaves the handle's 6px hitbox. No `on_mouse_up` needed:
            // GPUI's own dispatch clears `active_drag` on any `MouseUpEvent` regardless of which
            // element's handlers fired (`vendor/zed/crates/gpui/src/window.rs`'s
            // `dispatch_mouse_event`). `Self::apply_pane_resize` derives the width fresh from
            // the cursor position each tick, so there's no armed baseline to leak.
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<PaneResizeDrag>, _window, cx| {
                    let PaneResizeDrag(target) = *event.drag(cx);
                    this.apply_pane_resize(target, event.event.position.x, cx);
                },
            ))
            // Captures the body's paint bounds into `Self::body_bounds` every render, the same
            // `gpui::canvas` pattern `vendor/zed/crates/workspace/src/workspace.rs` uses for its
            // own dock-resize `bounds` field.
            .child({
                let this = cx.entity();
                gpui::canvas(
                    move |bounds, _window, cx| {
                        this.update(cx, |this, _cx| {
                            this.body_bounds = bounds;
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
            })
            .child(
                div()
                    .id("worktree-sidebar")
                    .relative()
                    .flex_none()
                    .w(self.rail_width)
                    .h_full()
                    .bg(theme::surface::RAIL)
                    .border_r_1()
                    .border_color(theme::border::ZONE)
                    .child(self.render_rail(cx))
                    .child(self.render_resize_handle(ResizeTarget::Rail, cx)),
            )
            .child(self.render_center_pane(cx))
            .child(
                div()
                    .id("file-tree-sidebar")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(self.panel_width)
                    .h_full()
                    .bg(theme::surface::RAIL)
                    .border_l_1()
                    .border_color(theme::border::ZONE)
                    .child(self.render_right_sidebar(cx))
                    .child(self.render_resize_handle(ResizeTarget::Panel, cx)),
            )
    }
}

/// The real "where to restore keyboard focus to once this overlay closes, and whether that
/// target is still safe to use" state each of this app's three focus-capturing overlays needs:
/// the code/file Surface C ([`AdeApp::code_focus`]), the command palette
/// ([`AdeApp::palette_focus`]), and Settings ([`AdeApp::settings_focus`]).
///
/// ## The dangling-focus invariant
///
/// Every one of these overlays replaces or covers whatever was rendered before it, and GPUI
/// resolves a dispatched action against whichever node the focused `FocusId` maps to in the
/// *last rendered frame* (`vendor/zed/crates/gpui/src/window.rs`'s
/// `focus_node_id_in_rendered_frame`), falling back to the dispatch tree's root node - above
/// every `on_action` handler - when that `FocusId` isn't found there. So: an overlay's `open_*`
/// must always end by moving `Window::focus` onto the overlay's own handle, and its `close_*`
/// must always end by calling [`restore_focus`] to move focus back onto something still
/// rendered, never leave it pointing at the now-unrendered overlay handle. This project has hit
/// the "close_* forgot to restore" version of this bug repeatedly (see BUILD-LOG.md); this type
/// and [`restore_focus`] are the single consolidated fix - every overlay's open/close pair
/// should route through them rather than hand-rolling capture/restore again.
#[derive(Default)]
pub(super) struct OverlayFocus {
    /// The focus target in place immediately before this overlay opened (`window.focused(cx)`,
    /// `None` on a fresh window) - restored by [`restore_focus`] on close.
    return_focus: Option<FocusHandle>,
    /// Which session was active when [`Self::capture`] ran - compared against the active
    /// session at restore time so [`restore_focus`] can tell whether `return_focus` is still
    /// safe to restore (it may belong to a session that's no longer active).
    opened_session: Option<SessionId>,
}

impl OverlayFocus {
    /// Records the current focus target and active session. Callers that must only capture on
    /// a genuine closed-to-open transition (not every subsequent navigation while already open -
    /// see [`AdeApp::focus_code_surface`]) guard the call themselves; this always captures
    /// unconditionally when called.
    pub(super) fn capture(&mut self, window: &Window, sessions: &Sessions, cx: &App) {
        self.return_focus = window.focused(cx);
        self.opened_session = sessions.active_id();
    }

    /// Discards captured state without restoring it - used only by
    /// [`AdeApp::close_palette`]'s Settings-showing-underneath branch, which moves focus onto
    /// `settings_focus_handle` directly instead of through [`restore_focus`].
    pub(super) fn clear(&mut self) {
        self.return_focus = None;
        self.opened_session = None;
    }
}

/// The shared focus-restore-on-close step for every overlay that captured a pre-open target via
/// [`OverlayFocus::capture`] - see this type's own docs for the invariant this closes.
///
/// If the active session changed while the surface was open, the captured handle is skipped in
/// favor of the *current* active session's terminal pane (a handle from a no-longer-active
/// session would be just as dangling as the overlay's own). Otherwise the captured handle is
/// restored, falling back to the active session's pane if nothing was focused before. A free
/// function, not an `AdeApp` method, since every caller already holds `&mut self` and needs to
/// pass `&mut self.some_field` alongside it. Deliberately doesn't call `cx.notify()` - every
/// caller has its own surface-specific state change around this call and issues its own single
/// `cx.notify()` once everything, this restore included, is done.
fn restore_focus(
    sessions: &Sessions,
    overlay_focus: &mut OverlayFocus,
    window: &mut Window,
    cx: &mut App,
) {
    let session_changed = sessions.active_id() != overlay_focus.opened_session;
    let restore_target = if session_changed {
        None
    } else {
        overlay_focus.return_focus.take()
    };
    let focus_target = restore_target.or_else(|| {
        sessions
            .active()
            .map(|session| session.pane.focus_handle(cx))
    });
    if let Some(handle) = focus_target {
        window.focus(&handle, cx);
    }
    overlay_focus.clear();
}

/// Regression coverage for the settings-save ordering race described on
/// [`AdeApp::_settings_save_task`]'s docs: two independent per-edit tasks sharing one
/// superseding `Option<Task<()>>` slot could let an older edit's `std::fs::write` complete
/// *after* a newer edit's, since dropping a `Task` cannot stop a write that already started.
/// [`AdeApp::persist_settings`]'s serial writer loop closes this structurally.
///
/// ## What these tests can and can't actually prove
///
/// Under GPUI's deterministic test executor, a background closure with no internal `.await`
/// points (like `Settings::save_at`) either hasn't been polled yet or runs to completion in one
/// synchronous step (`vendor/zed/crates/scheduler/src/test_scheduler.rs`'s `step_filtered`) -
/// there's no wall-clock thread pool making independent progress between test statements. So
/// "prove two writes were interleaved mid-write" isn't an achievable test goal here, and a test
/// that only checks the final on-disk value after two edits with the *same* injected delay would
/// pass identically whether or not writes are actually serialized (same-delay timers drain in
/// queuing order regardless).
///
/// What *is* testable is *ordering*: give an earlier edit a *longer* delay than a later edit
/// queued behind it. A non-serialized implementation would let the later, shorter-delayed write
/// land first and the earlier, now-stale write land after it, corrupting the file. Against the
/// real serial writer loop, no second write can begin until the first's delay-then-write fully
/// completes, so this inversion is structurally impossible.
#[cfg(test)]
mod settings_persist_tests {
    use super::*;
    use gpui::TestAppContext;
    use std::time::Duration;

    fn open_test_app_with_real_settings_path(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                repo_path,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    /// Edit 1 gets a long delay and is still pending when edit 2 is queued behind it with a
    /// shorter delay. A per-edit-independent-task implementation would let edit 2's write finish
    /// first and edit 1's stale write land on top afterward; the serial writer loop can't start
    /// edit 2's delay until edit 1's delay-then-write has fully completed.
    #[gpui::test]
    fn a_later_edit_queued_while_an_earlier_one_is_delayed_is_never_overwritten_by_it(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_path.clone(),
        );

        // Edit 1: a long delay, so its write is still genuinely pending when edit 2 is queued
        // below.
        let long_delay = Duration::from_millis(100);
        app.update(cx, |app, cx| {
            app.set_settings_save_test_delay(Some(long_delay));
            app.settings.appearance.editor_font_size = 11.0;
            app.persist_settings(cx);
        });
        assert!(app.read_with(cx, |app, _| app.settings_save_running));

        // Parks at the loop's delay timer for edit 1 (the delay is awaited before `save_at` is
        // called) - it can't resume until `advance_clock` below reaches the delay's expiration.
        cx.run_until_parked();

        // Edit 2, queued while edit 1's write is still pending, with a shorter delay - the shape
        // needed to expose an out-of-order write.
        let short_delay = Duration::from_millis(5);
        app.update(cx, |app, cx| {
            app.set_settings_save_test_delay(Some(short_delay));
            app.settings.appearance.editor_font_size = 22.0;
            app.persist_settings(cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.settings_save_pending),
            "the second edit should be recorded as pending, not dropped"
        );

        // Drain both delays, in whatever order the implementation actually resolves them.
        let mut settled = false;
        for _ in 0..40 {
            cx.background_executor
                .advance_clock(Duration::from_millis(5));
            cx.run_until_parked();
            if !app.read_with(cx, |app, _| app.settings_save_running) {
                settled = true;
                break;
            }
        }
        assert!(settled, "the serial writer loop never settled back to idle");
        assert!(!app.read_with(cx, |app, _| app.settings_save_pending));

        let on_disk = settings_store::Settings::load_or_init_at(&settings_path);
        assert_eq!(
            on_disk.appearance.editor_font_size, 22.0,
            "the file must hold edit 2's value - a real bug in the class this loop fixes would \
             let edit 1's own, longer-delayed write land on disk *after* edit 2's \
             shorter-delayed one, silently reverting it"
        );
    }

    /// A rapid burst of edits (far more than two), each given a *shorter* delay than the one
    /// before it and all queued before the loop has been driven forward even once, must still
    /// converge on exactly the final edit's value - not, as a non-serialized implementation
    /// would produce, whichever edit happened to carry the longest delay (here, deliberately the
    /// first) winning by finishing last. See
    /// [`a_later_edit_queued_while_an_earlier_one_is_delayed_is_never_overwritten_by_it`] for the
    /// two-edit version.
    #[gpui::test]
    fn a_burst_of_edits_with_decreasing_delays_converges_on_the_final_value(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_path.clone(),
        );

        app.update(cx, |app, cx| {
            // Deliberately decreasing delays: if each edit's write ran as its own independent
            // operation, the *first* (here longest-delayed, most stale) edit would be the last
            // to actually finish writing, landing its stale value on disk after every other one.
            for (size, delay_ms) in [
                (10.0, 60u64),
                (12.0, 50),
                (14.0, 40),
                (16.0, 30),
                (18.0, 20),
                (20.0, 10),
            ] {
                app.set_settings_save_test_delay(Some(Duration::from_millis(delay_ms)));
                app.settings.appearance.editor_font_size = size;
                app.persist_settings(cx);
            }
        });

        let mut settled = false;
        for _ in 0..40 {
            cx.background_executor
                .advance_clock(Duration::from_millis(10));
            cx.run_until_parked();
            if !app.read_with(cx, |app, _| app.settings_save_running) {
                settled = true;
                break;
            }
        }
        assert!(settled, "the serial writer loop never settled back to idle");
        assert!(!app.read_with(cx, |app, _| app.settings_save_pending));

        let on_disk = settings_store::Settings::load_or_init_at(&settings_path);
        assert_eq!(
            on_disk.appearance.editor_font_size, 20.0,
            "the file must hold the LAST edit's value, never an earlier, longer-delayed edit's \
             stale one landing on top of it afterward"
        );
    }
}

mod code_surface;
mod editing;
mod focus;
mod lsp;
mod merge_flow;
mod merge_flow_render;
mod palette_render;
mod rail_render;
mod rem_scope;
mod resize;
mod settings_render;
mod settings_widgets;
mod sidebar_render;
mod state;
mod status_bar;
mod task_pool;
mod title_bar;
mod widgets;
mod work_surface_render;
