//! The top-level three-pane window: a left worktree sidebar, a tabbed center pane of
//! terminal sessions, and a right file tree, composed as GPUI entities.
//!
//! ## What lives here, and what doesn't
//!
//! This module is the *app shell*, not a grab-bag of rendering code. It owns [`AdeApp`]
//! itself (the one state struct every subsystem reads and mutates) and its construction
//! ([`state`]), the crate's GPUI actions, the `Render` impl that composes the zones, and
//! the genuinely cross-zone mechanics: focus/overlay discipline ([`focus`], [`OverlayFocus`]), pane resizing
//! ([`resize`] plus its pure width-clamp half, [`layout`]), the scoped rem-size override
//! Surface C's zoom paints through ([`rem_scope`]), the shared keycap/chip/message widgets
//! ([`widgets`]), the background-task slot type ([`task_pool`]), and the "New file" prompt
//! ([`new_file`]), which is an overlay reachable from two different zones and so belongs to
//! neither.
//!
//! Everything *about one subsystem* lives in that subsystem's own folder instead - both its
//! pure, window-free logic and the `impl AdeApp` blocks that draw it: `crate::rail`,
//! `crate::work_surface`, `crate::sidebar`, `crate::code_surface`, `crate::merge`,
//! `crate::palette`, `crate::settings`, `crate::status_bar`, `crate::title_bar`,
//! `crate::terminal`, `crate::lsp`, `crate::worktree_history`. So "everything about
//! feature X" is one directory, not two unrelated ones.
//!
//! ## Offloading `wt-core`'s blocking calls
//!
//! `wt_core::list_worktrees` performs blocking I/O (`gix` object-database reads, and
//! sometimes spawning `git`). It's never called directly from `render` or an event handler;
//! [`AdeApp::load_worktrees`] hands it to `cx.background_executor().spawn(..)` and only
//! touches `self` again inside a `this.update(cx, ..)` callback once the background task
//! resolves. `crate::sidebar::file_tree::build_file_tree`'s `std::fs::read_dir` walk follows the same
//! pattern.
//!
//! ## One rail row per worktree; sessions are tabs scoped to it
//!
//! [`crate::work_surface::sessions::Sessions`] holds any number of independent, simultaneously-running
//! terminal sessions (a plain shell, or an agent CLI), each pinned to the worktree it was
//! started in. The session rail shows exactly one row per worktree
//! (`crate::rail::state::WorktreeRow`, aggregating every session open in it), and the centre pane's tab
//! strip (`AdeApp::render_tab_strip`) only ever shows the *currently selected* worktree's own
//! sessions - never a flat, unscoped list of every session across every worktree.
//!
//! Selecting a worktree in the sidebar still never spawns or kills anything - but, unlike
//! before this revision, it *does* change which session is "active"
//! (`crate::work_surface::sessions::Sessions::activate_for_worktree`, called from [`AdeApp::select_worktree`]):
//! the active session must always belong to the selected worktree, or the centre pane would show
//! one worktree's terminal while the rail highlights another. [`AdeApp::selected`] itself still
//! drives the file tree, and which worktree `active_session_cwd` resolves to for the *next* "New
//! terminal"/"New agent pane" click - that part is unchanged. Spawning a session is still always
//! its own explicit action, never an implicit side effect of browsing.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, font, prelude::*, px, App, ClickEvent, Context, DragMoveEvent, Empty,
    FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent, Pixels, Subscription, Task,
    UniformListScrollHandle, Window,
};
use wt_core::diff::DiffFile;
use wt_core::merge::ConflictHunk;

use crate::code_surface::code_view;
use crate::code_surface::edit_buffer;
use crate::env_info;
use crate::keymap::WindowControlsStyle;
use crate::keymap_overrides;
use crate::lsp::diagnostics as diagnostics_view;
use crate::merge::state as merge;
use crate::palette::state as palette;
use crate::rail::state::{self as rail, RailMode};
use crate::rail::worktrees::{self, WorktreeItem};
use crate::settings::custom_theme;
use crate::settings::state as settings;
#[cfg(test)]
use crate::settings::state::SettingsPage;
use crate::settings::store::{self as settings_store, CfgFormat, Settings};
use crate::sidebar::changes::{self, ChangeTag};
use crate::sidebar::file_tree::{self, FileTreeEntry};
use crate::sidebar::fold_state;
use crate::sidebar::tree_ops;
use crate::status_bar::process_stats;
use crate::text_history;
use crate::theme;
use crate::title_bar::menu as title_bar;
use crate::work_surface::sessions::{SessionId, SessionKind, Sessions};
use crate::work_surface::state as work_surface;
use crate::worktree_history::flow as worktree_history;
use crate::worktree_history::undo;

use crate::code_surface::state::{
    BlameCacheEntry, BlameLoadState, CommitMessageState, DiffLoadState, FileLoadState, HoverEntry,
};
use crate::lsp::client::LspClientState;
use crate::lsp::completion_popup::CompletionsEntry;
use crate::root::resize::{PaneResizeDrag, ResizeTarget};
use crate::root::task_pool::TaskPool;
use crate::sidebar::render::RightSidebarView;

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
// `crate::code_surface::editing`'s `EntityInputHandler` impl and action handlers. Bound with a
// `key_context` of `Some("file-editor")` in `crate::default_key_bindings`, scoped to only the
// editable File view container (`crate::code_surface::file_view::render_file_view`'s inner container,
// not the shared outer Diff/File surface `.key_context("diff")` also uses) - see that function's
// own docs for why the Diff view must never receive these bindings.
//
// `Completions*` (Revision R8.5b) navigate/accept/dismiss the real Completions popup
// (`crate::lsp::completion_popup`). Bound with `Some("file-editor && completions")` - a real,
// *narrower* context than the plain `Editor*` bindings above, added to the same code-surface node
// only while `AdeApp::completions` is genuinely `Ready` (see `crate::code_surface::
// AdeApp::render_code_surface`'s own docs, and `crate::lsp::completion_popup::AdeApp::
// completions_open_for_active_path`'s own docs for why a merely `Loading`/`Failed` entry does
// *not* count - a real, live-reproduced keystroke-swallowing bug this project's own audit caught,
// see that method's docs) - and the plain `up`/`down`/`enter` `Editor*` bindings are
// correspondingly narrowed to `Some("file-editor && !completions")`, so the two sets can never
// both match the same keystroke. This is the same real `KeyBindingContextPredicate` mechanism
// (`&&`, `!`) Revision R8.5a's own `"]"`/`"diff && !file-editor"` fix already established for this
// exact bug class - see that binding's own docs in `crate::default_key_bindings`.
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
        EditorWordLeft,
        EditorWordRight,
        EditorSelectWordLeft,
        EditorSelectWordRight,
        EditorHome,
        EditorEnd,
        EditorSelectAll,
        EditorCopy,
        EditorCut,
        EditorPaste,
        EditorSave,
        EditorSaveAnyway,
        EditorSelectNextOccurrence,
        EditorSelectAllOccurrences,
        EditorSkipOccurrence,
        EditorCollapseCursors,
        CompletionsUp,
        CompletionsDown,
        CompletionsAccept,
        CompletionsDismiss,
        CompletionsInvoke,
        EditorIndent,
        EditorDedent,
        EditorEscape,
        Undo,
        Redo,
        TextUndo,
        TextRedo,
        CloseFocusedTab,
        FileTreeContextMenu,
        FileTreeRename,
        FileTreeCopy,
        FileTreeCut,
        FileTreePaste,
    ]
);

/// How often `crate::rail::state::compute_status_snapshot`'s background `git` status/diff refresh
/// re-runs. Coarser than `crate::terminal::pane`'s 8ms poll since this spawns real `git` child
/// processes per worktree/session path, not a cheap channel `try_recv`.
pub(crate) const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often [`AdeApp::render_file_view`] calls `std::fs::metadata` for its freshness check -
/// throttled rather than unconditional-per-render (see
/// [`AdeApp::file_view_last_freshness_check`]).
pub(crate) const FILE_FRESHNESS_CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on how many changed files the diff view renders, independent of `wt_core::diff`'s own
/// `MAX_FILES` cap (300) on the loaded diff. The Files tree used to have a matching
/// render cap; it no longer does (GitHub issue #18 §4), so this one now stands alone.
pub(crate) const MAX_RENDERED_DIFF_FILES: usize = 40;

/// Cap on how many hunk lines a single file's diff renders, independent of `wt_core::diff`'s
/// own per-file `MAX_HUNK_LINES_PER_FILE` cap (2000) on loaded data.
pub(crate) const MAX_RENDERED_DIFF_LINES_PER_FILE: usize = 300;

/// The Diff view's per-hunk syntax-highlight + gutter-number cache's real shape - see
/// [`AdeApp::diff_highlight_cache`]'s own docs for what each element means and why the `DiffFile`
/// is a real identity guard, not decoration. A named `type` (rather than the bare tuple type
/// inline) so `clippy::type_complexity` doesn't fire, and so `code_surface`'s own
/// `diff_highlight_cache_for` can share the exact same shape as the field it reads.
pub(crate) type DiffHighlightCache = (
    DiffFile,
    Vec<Vec<code_view::RenderedLine>>,
    Vec<Vec<(Option<usize>, Option<usize>)>>,
);

/// How many times [`AdeApp::persist_fold_state`]'s writer loop retries a failing write before
/// giving up on it. Bounded so a permanently broken path (a read-only `~/.config`, say) can't
/// spin the loop forever; the next real expand/collapse starts a fresh budget, since a new user
/// action is the honest trigger for trying again.
pub(crate) const FOLD_STATE_SAVE_MAX_ATTEMPTS: u32 = 5;

/// Multiplied by the attempt number for [`AdeApp::persist_fold_state`]'s linear retry backoff.
pub(crate) const FOLD_STATE_SAVE_RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// How often [`AdeApp::ensure_lsp_poll_task`]'s background loop checks for a newly-arrived
/// `publishDiagnostics` notification. Coarser than `crate::terminal::pane::POLL_INTERVAL` (8ms):
/// pty output is latency-sensitive, rust-analyzer's diagnostics are not.
pub(crate) const LSP_DIAGNOSTICS_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long [`AdeApp::request_hover`]/[`AdeApp::trigger_goto_definition`] wait for
/// rust-analyzer's response before giving up. Both run against an already-`Ready`
/// [`LspClientState`], so this budgets one query's round trip, not indexing from a cold start.
pub(crate) const LSP_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct AdeApp {
    pub(crate) repo_path: PathBuf,
    pub(crate) worktrees: Vec<WorktreeItem>,
    pub(crate) worktrees_error: Option<String>,
    /// The session rail's own real overlay scrollbar handle (GitHub issue #30) - a plain
    /// `gpui::ScrollHandle`: `crate::rail::render::AdeApp::render_rail_list` renders every row
    /// eagerly, not through a `uniform_list`.
    pub(crate) rail_scroll_handle: gpui::ScrollHandle,
    pub(crate) selected: Option<usize>,
    pub(crate) sessions: Sessions,
    pub(crate) file_tree: Vec<FileTreeEntry>,
    pub(crate) file_tree_root: PathBuf,
    pub(crate) file_tree_error: Option<String>,
    pub(crate) right_sidebar_view: RightSidebarView,
    /// The file tree's own `uniform_list` scroll handle (GitHub issue #30) - real overlay
    /// scrollbar geometry (`crate::root::scrollbar::AdeApp::render_vertical_scrollbar`) is read
    /// straight off this handle's `base_handle` (`gpui::UniformListScrollHandle::0`), the same
    /// handle `crate::sidebar::render::AdeApp::render_file_tree`'s own `uniform_list` is
    /// `track_scroll`'d with - not a second, parallel tracking mechanism.
    pub(crate) file_tree_scroll_handle: UniformListScrollHandle,
    /// The Changes list's own equivalent of [`Self::file_tree_scroll_handle`] - a separate handle
    /// (not shared) because the two `uniform_list`s are mutually exclusive tabs of the same panel
    /// but never rendered at the same time, and giving them independent scroll state is what lets
    /// switching tabs and back restore each list's own scroll position rather than the other's.
    pub(crate) changes_rows_scroll_handle: UniformListScrollHandle,
    pub(crate) diff_root: PathBuf,
    pub(crate) diff_state: DiffLoadState,
    /// The real `+n`/`-n` totals across every file in [`Self::diff_state`]'s currently loaded
    /// diff (`Self::render_right_sidebar_toggle`'s header totals), computed once - off the UI
    /// thread, alongside `diff_state` itself becoming `DiffLoadState::Loaded` - rather than
    /// re-folded over every one of up to 300 files' hunks on *every single render* regardless
    /// of which Zone 3 tab is even showing. `None` whenever there's no loaded diff to sum (see
    /// [`Self::current_diff`]'s docs for exactly which [`DiffLoadState`]/[`DiffBase`]
    /// combinations count).
    pub(crate) diff_totals: Option<(u32, u32)>,
    /// Real expand/collapse state for the file tree - a directory's absolute path is in this set
    /// iff it is expanded (see `crate::sidebar::file_tree::visible_entries`, which this set feeds
    /// directly). **Absence means collapsed**, so a worktree opened for the first time shows only
    /// its root-level entries (GitHub issue #18 §1).
    ///
    /// This is the live, in-memory mirror of one worktree's entry in [`Self::fold_state`]; every
    /// mutation goes through `AdeApp::set_dir_expanded`/`collapse_all_dirs`/`reveal_in_tree`,
    /// which keep the two in step and write the change to disk immediately. Re-derived from
    /// `fold_state` on every worktree switch (`Self::select_worktree`), never carried across one.
    pub(crate) expanded_dirs: HashSet<PathBuf>,
    /// Every worktree's persisted fold state, loaded once at startup from
    /// `~/.config/jerry/file-tree-state.toml` - see `crate::sidebar::fold_state`'s module docs
    /// for the file's shape, its per-worktree-path keying, and why it is a separate file from
    /// `settings.toml` with a genuinely atomic write path.
    pub(crate) fold_state: fold_state::FoldState,
    /// The resolved path [`Self::persist_fold_state`] writes to - a sibling of
    /// [`Self::settings_path`], and `None` for exactly the same tests that get a `None` settings
    /// path, which makes a fold-state save a real no-op rather than a special-cased test skip.
    pub(crate) fold_state_path: Option<PathBuf>,
    /// [`Self::file_tree_root`]'s resolved `fold_state::worktree_key`, recomputed exactly once
    /// per real root change (`Self::set_file_tree_root`) rather than per lookup. That caching is
    /// not a micro-optimization: `worktree_key` calls `std::fs::canonicalize`, and the callers
    /// are an expand/collapse click and - once per ancestor - a "reveal in tree", so resolving it
    /// on demand meant up to a dozen blocking syscalls on the foreground thread per gesture,
    /// which on a stale NFS/FUSE mount is a frozen window rather than a slow one. `None` when the
    /// root isn't valid UTF-8 (see `worktree_key`), which makes every record attempt a logged
    /// refusal instead of a silent mis-key.
    pub(crate) fold_state_root_key: Option<String>,
    /// The `fold_state::worktree_key`s this instance has recorded anything for - what
    /// [`Self::persist_fold_state`] hands `FoldState::save_merged_at` as "mine to overwrite".
    /// Every other key in the file belongs to some other running instance (one `jerry` process
    /// per repository) and is passed through untouched; see `crate::sidebar::fold_state`'s
    /// module docs for the whole-file-clobber this exists to prevent.
    pub(crate) fold_state_owned: std::collections::BTreeSet<String>,
    /// The fold-state file's serial writer loop - the exact same mechanism (and the same
    /// reasoning) as [`Self::_settings_save_task`], just for the other file. Two writes to one
    /// path are never allowed to overlap; a change that lands while a write is in flight is
    /// picked up by the still-running loop.
    pub(crate) _fold_state_save_task: Option<Task<()>>,
    /// See [`Self::settings_save_pending`] - same contract, for the fold-state file.
    pub(crate) fold_state_save_pending: bool,
    /// See [`Self::settings_save_running`] - same contract, for the fold-state file.
    pub(crate) fold_state_save_running: bool,
    /// Whether the last completed file-tree walk stopped early at its configured entry cap
    /// (`Settings.file_tree.max_entries`, or [`Self::file_tree_limit_override`]). Drives the
    /// sidebar's real "load more" action - the explicit replacement for the removed
    /// "... and N more entries not shown" row.
    pub(crate) file_tree_truncated: bool,
    /// Whether that walk was a *complete* inventory of the worktree's directories
    /// (`file_tree::FileTreeListing::is_complete`) - false when it was truncated, and also when
    /// it silently skipped an unreadable or too-deep subdirectory. The only condition under
    /// which `AdeApp::prune_stale_fold_state` may read "not in this listing" as "deleted".
    pub(crate) file_tree_complete: bool,
    /// Set by the sidebar's "load more" action (`Self::load_more_file_tree_entries`): the cap
    /// this worktree's walks use instead of `Settings.file_tree.max_entries`, raised tenfold per
    /// click. Deliberately still a cap and never `None`: a single click that re-walked a
    /// bind-mounted `$HOME` with an unbounded budget would allocate millions of `PathBuf`s on a
    /// background thread and then hand them all to `rebuild_palette_file_candidates`. Each click
    /// raises the bound and the row keeps reporting where the walk stopped, so the listing is
    /// never *silently* cut off - which is what issue #18 §4 actually asks for. Session-scoped
    /// and per-worktree: reset on every worktree switch, since "show me more of *this* tree"
    /// says nothing about the next one.
    pub(crate) file_tree_limit_override: Option<usize>,
    /// The file tree's open right-click context menu (GitHub issue #19 §1), `None` when closed -
    /// see `crate::sidebar::tree_ops::TreeContextMenu`.
    pub(crate) tree_context_menu: Option<tree_ops::TreeContextMenu>,
    /// The file tree's in-progress inline name editor (New File / New Folder / Rename), `None`
    /// when none is open. Held here rather than inside [`Self::file_tree`] on purpose - see
    /// `crate::sidebar::tree_ops::TreeInlineEdit`'s own docs for the watcher-refresh race that
    /// placement closes (issue #19 §4).
    pub(crate) tree_inline_edit: Option<tree_ops::TreeInlineEdit>,
    /// The tree's own cut/copy buffer - a real filesystem entry, deliberately not the system
    /// clipboard (see `crate::sidebar::tree_ops::TreeClipboard`).
    pub(crate) tree_clipboard: Option<tree_ops::TreeClipboard>,
    /// A delete that has been requested but not yet confirmed. Nothing is ever removed while
    /// this is merely `Some`; `crate::sidebar::tree_ops::AdeApp::confirm_tree_delete` is the only
    /// path that acts, and it is only reachable from the confirmation panel's own button.
    pub(crate) tree_delete_confirm: Option<tree_ops::PendingTreeDelete>,
    /// The most recent file-operation failure (a refused rename, a failed trash command),
    /// surfaced under the tree rather than dropped into the log - the same small, honest error
    /// surface [`Self::file_save_error`] uses for a failed save.
    pub(crate) tree_op_error: Option<String>,
    /// The file tree's keyboard-focus target. `track_focus`'d by
    /// `crate::sidebar::render::AdeApp::render_file_tree`'s container, which is also the node
    /// carrying the `"file-tree"` `key_context` every tree keybinding is scoped to - so
    /// `Ctrl+C`/`Ctrl+X`/`Ctrl+V` can never match while a terminal session has focus. See
    /// `crate::sidebar::tree_ops`'s module docs.
    pub(crate) tree_focus_handle: FocusHandle,
    /// The file tree container's real painted bounds, captured by a `gpui::canvas` child each
    /// render (the same pattern [`Self::plus_button_bounds`] uses) - where a *keyboard*-opened
    /// context menu (`Shift+F10`) anchors, since there is no cursor position to use.
    pub(crate) file_tree_bounds: gpui::Bounds<Pixels>,
    /// The in-flight confirmed delete (a real `gio trash` child process, or a real
    /// `remove_dir_all`), one slot: a second delete can't be requested until the confirmation
    /// panel is open again, so there is never more than one.
    pub(crate) _tree_delete_task: Option<Task<()>>,
    /// The in-flight Duplicate / paste-a-copy - a real, recursive `std::fs` tree copy, run on the
    /// background executor rather than in the click listener that started it (see
    /// `crate::sidebar::tree_ops::AdeApp::spawn_tree_copy`). One slot, superseding: a second copy
    /// started while one is running drops the first *task handle*, which cannot stop a copy
    /// already in progress - deliberately, since abandoning one half-way is strictly worse than
    /// letting it finish, and the two have different destinations (each is
    /// `file_ops::unique_destination`-resolved) so they cannot collide with each other.
    pub(crate) _tree_copy_task: Option<Task<()>>,
    /// Per-file "reviewed" toggle state for the Changes list - a file's path is in this set iff
    /// its checkbox is checked. No backend "review" concept exists yet; this is purely local UI
    /// state that `Self::render_changes_header`'s progress bar and count read directly.
    pub(crate) reviewed_files: HashSet<PathBuf>,
    /// Ordered list of currently-open file tabs, rendered after every session's own tab by
    /// `Self::render_tab_strip`. No duplicates: opening an already-open file just activates its
    /// existing entry (`Self::push_open_file`). Removed only on explicit tab close
    /// (`Self::close_file_tab`) or leaving the owning worktree (`reset_per_worktree_ui_state`) -
    /// these are worktree-relative paths, meaningless (or collision-prone) once the worktree
    /// changes.
    pub(crate) open_files: Vec<PathBuf>,
    /// Which file tab (if any) the centre pane is showing instead of a session -
    /// `Some(path)` iff `path` is also in [`Self::open_files`]. Set by a Changes row
    /// (`Self::open_change_diff`), a Files-tree row (`Self::open_file_view`), or an already-open
    /// tab (`Self::activate_file_tab`); cleared by selecting a session tab or closing the active
    /// tab down to none left.
    pub(crate) open_change: Option<PathBuf>,
    /// `Some(path)` for one real "arming" click/keystroke on a *dirty* file tab's close
    /// affordance (`×`, middle-click, or `Ctrl+W` - GitHub issue #26), cleared by the confirming
    /// second gesture on the same `path` (which then really closes it) or by most other tab/file
    /// navigation in the meantime - the same real two-gesture confirmation idiom
    /// [`Self::prune_confirm_armed`]/[`Self::discard_confirm_armed`] already establish for this
    /// app's other destructive-feeling actions. A clean (non-dirty) tab never arms this at all -
    /// see [`crate::code_surface::tabs::AdeApp::request_close_file_tab`]'s own docs for why an
    /// unsaved-changes prompt for a tab with nothing unsaved would be real, unnecessary friction.
    pub(crate) close_tab_confirm_armed: Option<PathBuf>,
    /// Cached `DiffFile` for whichever path [`Self::open_change`] names (`None` if it has no
    /// diff, or nothing is open) - kept fresh by [`Self::refresh_open_diff_file_cache`] instead
    /// of re-cloning the whole diff (up to 2000 hunk lines) on every render.
    /// `Self::render_center_pane` moves it out (`Option::take`) rather than cloning it again
    /// before calling `Self::render_code_surface` (which needs `&mut self`) and moves it back
    /// afterward - an O(1) swap, not a second deep clone.
    pub(crate) open_diff_file_cache: Option<DiffFile>,
    /// File-tree path last resolved from a palette file result with no diff to open
    /// (`Self::open_palette_file_result`) - highlighted in `Self::render_file_tree_row` like a
    /// Changes row's own selection highlight.
    pub(crate) selected_tree_path: Option<PathBuf>,
    /// Surface C's `Diff | File` toggle for whichever file [`Self::open_change`] names - set to
    /// `Diff` by [`Self::open_change_diff`] and `File` by [`Self::open_file_view`], read by
    /// [`Self::render_code_surface`] alongside a "does this file even have a diff" check (a
    /// diff-less file always renders as `File` regardless of this field).
    pub(crate) code_view: code_view::CodeView,
    /// Surface C's Diff/File focus target, `track_focus`'d by
    /// [`Self::render_code_surface`]'s outer container - see [`OverlayFocus`]/[`restore_focus`]
    /// for the dangling-focus invariant this and [`Self::code_focus`] exist to satisfy.
    pub(crate) code_focus_handle: FocusHandle,
    /// Pre-open focus target for [`Self::code_focus_handle`] - see [`OverlayFocus`].
    pub(crate) code_focus: OverlayFocus,
    /// Real, shared caret blink state (GitHub issue #27) - see `crate::root::caret_blink`'s
    /// module docs for the whole mechanism. `true` means the caret is in its "on" (painted)
    /// phase right now; every caret-bearing surface's own render call site
    /// (`crate::code_surface::editing::render_editable_file_view_line`) reads this alongside its
    /// own `FocusHandle::is_focused` check, so a caret only actually blinks while it is both the
    /// real live caret *and* genuinely focused.
    pub(crate) caret_blink_visible: bool,
    /// The live blink loop, restarted by [`Self::reset_caret_blink`]/
    /// [`Self::start_caret_blink`] on every real cursor-moving action, edit, or focus change so a
    /// stale timer can never fire after the caret it was blinking has moved on -
    /// `Task::ready(())` (already-finished, fires nothing) is the real idle value.
    pub(crate) _caret_blink_task: Task<()>,
    /// `cx.on_focus`/`cx.on_blur` subscriptions on every real caret-bearing `FocusHandle` this
    /// app has, wired once in [`Self::new_with_settings`] - see
    /// `crate::root::caret_blink::AdeApp::wire_caret_blink`. Held for this instance's whole
    /// lifetime; an unheld `gpui::Subscription` is dropped, and a dropped one stops firing.
    pub(crate) _caret_blink_subscriptions: Vec<Subscription>,
    /// The File view's `uniform_list` scroll handle (`gpui::UniformListScrollHandle`, matching
    /// `vendor/zed/crates/git_ui/src/git_panel.rs`'s `commit_history_scroll_handle` use of the
    /// same type) - driven by go-to-definition landing on a distant [`Self::code_cursor`] line,
    /// never on an ordinary click or fresh file open (no reason to fight the user's own scroll
    /// position).
    pub(crate) file_view_scroll_handle: UniformListScrollHandle,
    /// The read-only Diff view's own real overlay scrollbar handle (GitHub issue #30) - a plain
    /// `gpui::ScrollHandle`: `crate::code_surface::diff_view::AdeApp::render_diff_file_detail`
    /// renders every hunk line eagerly into a plain `overflow_y_scroll()` div, not a
    /// `uniform_list`, so it needs the base handle type directly rather than the `uniform_list`
    /// wrapper [`Self::file_view_scroll_handle`] uses.
    pub(crate) diff_view_scroll_handle: gpui::ScrollHandle,
    /// Cached parse/highlight of whichever file [`Self::render_file_view`] last loaded
    /// (`code_view::load_file`/`highlight_rust`) - reused unless `code_view::cache_is_fresh`
    /// says otherwise, always written from [`Self::spawn_file_load`]'s background task, never
    /// synchronously during `render()`.
    pub(crate) file_view_cache: Option<code_view::ParsedFile>,
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
    pub(crate) diff_highlight_cache: Option<DiffHighlightCache>,
    /// Path and time [`Self::render_file_view`] last called `std::fs::metadata` for its
    /// freshness check, throttling that syscall to at most once per
    /// [`FILE_FRESHNESS_CHECK_INTERVAL`] instead of every render. `None` until the first check;
    /// `Self::select_worktree` doesn't need to reset this since a worktree switch always changes
    /// `file_tree_root`, forcing a path mismatch and thus a fresh check anyway.
    pub(crate) file_view_last_freshness_check: Option<(PathBuf, Instant)>,
    /// See [`FileLoadState`]'s own docs.
    pub(crate) file_load_state: FileLoadState,
    /// Changed-line set (`code_view::changed_line_set`) for whichever `DiffFile`
    /// [`Self::open_diff_file_cache`] holds - recomputed only by
    /// [`Self::refresh_open_diff_file_cache`], never per render.
    pub(crate) file_view_changed_lines: HashSet<usize>,
    /// The real minimap panel's most recently measured bounds (`crate::code_surface::minimap`) -
    /// updated every render by a small measuring `gpui::canvas` child, the same established
    /// one-frame-lag idiom [`Self::body_bounds`]/[`Self::plus_button_bounds`] already use for the
    /// same real reason (an absolutely-positioned sibling - here, the viewport slider - needs a
    /// real pixel height that's only known after layout). `gpui::Bounds::default()` (zero) until
    /// the first real measurement lands.
    pub(crate) minimap_panel_bounds: gpui::Bounds<Pixels>,
    /// The File view's "last click" cursor line (1-indexed), set by
    /// [`Self::render_file_view_line`]'s click handler and reset to `1` on a fresh file load.
    /// No column tracking: per-character hit-testing against a monospace run wasn't implemented
    /// this phase, so no column is shown at all rather than a fabricated `col 1`.
    pub(crate) code_cursor: Option<usize>,
    /// Real, cached `wt_core::blame::blame_file` results (GitHub issue #29), keyed by absolute
    /// path - see `crate::code_surface::blame_view`'s own module docs for the threading/caching
    /// design and what "revision" means for freshness here.
    pub(crate) blame_cache: HashMap<PathBuf, BlameCacheEntry>,
    /// In-flight/settled state per absolute path, mirroring [`Self::file_load_state`]'s own
    /// single-flight discipline so [`Self::maybe_refresh_blame`] never dispatches a second
    /// background `git blame` for a path that already has one running.
    pub(crate) blame_state: HashMap<PathBuf, BlameLoadState>,
    pub(crate) _blame_tasks: HashMap<PathBuf, Task<()>>,
    /// Path and time [`Self::maybe_refresh_blame`] last rechecked blame freshness for, throttling
    /// that (potentially real-`git`-spawning) recheck the same way
    /// [`Self::file_view_last_freshness_check`] throttles the syntax-highlight one - see
    /// [`crate::code_surface::blame_view::BLAME_FRESHNESS_CHECK_INTERVAL`]'s own docs.
    pub(crate) blame_last_freshness_check: Option<(PathBuf, Instant)>,
    /// Real, full commit-message bodies (`wt_core::blame::commit_message`), keyed by commit sha
    /// rather than by file/line - a sha's message is the same regardless of which file/line
    /// referenced it, so this is shared across every open file. Populated lazily, only for a sha
    /// the current line's hover tooltip actually needs (see
    /// [`Self::ensure_blame_commit_message`]), off-thread.
    pub(crate) blame_commit_messages: HashMap<String, CommitMessageState>,
    pub(crate) _blame_message_tasks: HashMap<String, Task<()>>,
    /// Real, live per-tab text-editing state for the File view (Revision R8.5a) - keyed the same
    /// way as [`Self::open_files`] (a worktree-relative path), so switching between open file
    /// tabs never loses unsaved edits in a background tab. Created lazily the first time a file
    /// is opened in File view (see
    /// [`crate::code_surface::file_view::AdeApp::render_file_view`]), seeded from the exact same
    /// background read [`Self::spawn_file_load`] already performs. [`Self::file_view_cache`]
    /// stays the freshness-check/diagnostics/hover source of truth (the last-*saved* snapshot,
    /// per this phase's own scope); this map is what's actually on screen and what an explicit
    /// save writes. Deliberately **not** removed on an ordinary tab close
    /// ([`crate::code_surface::tabs::AdeApp::close_file_tab`]) - dropping unsaved edits just
    /// because a tab was closed (with no "save before closing?" prompt - out of scope this
    /// phase) would be a real, silent data-loss risk; reopening the same file later restores the
    /// exact in-memory buffer. Reset alongside `open_files` in `reset_per_worktree_ui_state` so a
    /// worktree switch doesn't leak another worktree's buffers, matching the same
    /// per-worktree-reset convention `open_files` already follows. (Editor zoom used to be
    /// reset the same way too - see `settings_store`'s "Editor zoom is one global, persisted
    /// number now" docs for why it no longer is: it moved to `Settings.appearance.
    /// editor_zoom_percent`, a real persisted field, not per-worktree UI state.)
    pub(crate) edit_buffers: HashMap<PathBuf, edit_buffer::EditBuffer>,
    /// Every visible File-view row's real painted bounds and shaped line, captured by
    /// `crate::code_surface::editing`'s per-row `gpui::canvas` paint callback each render - read back by
    /// a row's own click handler to hit-test a click into a real byte offset
    /// (`gpui::LineLayout::closest_index_for_x`) for real click-to-place-cursor. Keyed by 1-based
    /// line number (matching [`Self::code_cursor`]'s convention). Transient/best-effort: entries
    /// for rows no longer visible are simply never refreshed again (harmless - a stale entry is
    /// only ever read for a row-click hit test, and a scrolled-away row can't be clicked), so this
    /// is cleared wholesale only on a worktree switch, not pruned every frame.
    pub(crate) file_view_row_layout: HashMap<usize, (gpui::Bounds<Pixels>, gpui::ShapedLine)>,
    /// The real shaped line, bounds, and 0-indexed buffer line that painted the *caret's own* row
    /// most recently - `crate::code_surface::editing`'s `EntityInputHandler::bounds_for_range`/
    /// `character_index_for_point` read these three together (never `file_view_row_layout`, which
    /// only serves click hit-testing) and honestly return `None` when the caret's row wasn't
    /// actually painted last frame (e.g. scrolled out of view) - the same real, structural
    /// "degrade honestly when the query can't be answered" behavior
    /// `vendor/zed/crates/gpui/examples/input.rs`'s own `TextInput::last_layout`/`last_bounds`
    /// have, scoped here to whichever path/line they're actually for so a stale entry from a
    /// different file/line can never be misapplied.
    pub(crate) file_view_last_layout: Option<gpui::ShapedLine>,
    pub(crate) file_view_last_bounds: Option<gpui::Bounds<Pixels>>,
    /// The path and 0-indexed buffer line [`Self::file_view_last_layout`]/
    /// [`Self::file_view_last_bounds`] are for - `None` until the first paint of a caret row.
    pub(crate) file_view_last_layout_for: Option<(PathBuf, usize)>,
    /// Every in-flight debounced real re-highlight (`code_view`'s real `tree-sitter` parse) for a
    /// dirty [`Self::edit_buffers`] entry, keyed by the same relative path - see
    /// [`edit_buffer`]'s own "Re-highlighting cost" docs for why this is debounced rather than
    /// run inline on every keystroke. A single slot per path (not a `TaskPool`): only the most
    /// recent keystroke's debounce for a given file should ever fire, so a fresh keystroke
    /// re-arming the same path's entry correctly cancels (drops) whatever shorter-lived timer was
    /// still waiting - no risk of an out-of-order *write*, unlike
    /// [`Self::_file_save_tasks`]'s real disk writes, since this only ever reads
    /// `edit_buffers`/writes back into it, gated by [`crate::code_surface::edit_buffer::EditBuffer::
    /// apply_highlight`]'s own real content-snapshot equality check.
    pub(crate) _rehighlight_tasks: HashMap<PathBuf, Task<()>>,
    /// Every in-flight explicit `crate::code_surface::editing::AdeApp::save_active_file` background
    /// write, one slot per path - see [`Self::file_save_pending`]/[`Self::file_save_running`]'s
    /// own docs for the serial-writer-loop discipline (mirroring
    /// [`Self::_settings_save_task`]'s own, per-path rather than global) that keeps two saves of
    /// the *same* file from ever racing on disk, the exact class of bug Revision R5.5 fixed once
    /// for settings.
    pub(crate) _file_save_tasks: HashMap<PathBuf, Task<()>>,
    /// Paths with a save requested while that same path's serial writer loop
    /// ([`Self::_file_save_tasks`]) was already running - picked up by the loop's own next
    /// iteration rather than racing a second, independent `std::fs::write` against the same file.
    pub(crate) file_save_pending: HashSet<PathBuf>,
    /// Paths whose serial writer loop is currently alive - guards against spawning a second loop
    /// for a path that already has one draining [`Self::file_save_pending`].
    pub(crate) file_save_running: HashSet<PathBuf>,
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
    pub(crate) file_save_error: Option<(PathBuf, String)>,
    /// Paths currently flagged with a real, detected conflict: an unsaved (dirty) edit buffer
    /// whose underlying file has genuinely changed on disk since it was loaded/last saved (some
    /// other process - git, the agent CLI in a terminal tab, an external editor - touched it).
    /// Set by [`crate::code_surface::file_view::AdeApp::render_file_view`]'s existing freshness check
    /// when it fires while the buffer is dirty; cleared once the check no longer finds a mismatch.
    /// [`crate::code_surface::editing::AdeApp::save_active_file`] refuses to save (with its own,
    /// authoritative fresh `std::fs::metadata` check, not just this render-throttled flag) while
    /// a path is in this set - see that method's own docs for why silently overwriting the
    /// external change, or silently discarding the user's own unsaved edits, are both wrong.
    pub(crate) file_external_conflict: HashSet<PathBuf>,
    /// Whether the command palette (⌘P) overlay is open.
    pub(crate) palette_open: bool,
    /// The palette's own real overlay scrollbar handle (GitHub issue #30) - a plain
    /// `gpui::ScrollHandle`: `crate::palette::render::AdeApp::render_palette_groups` renders
    /// every result row eagerly, not through a `uniform_list`.
    pub(crate) palette_results_scroll_handle: gpui::ScrollHandle,
    /// The palette's active scope (`All`/`Commands`/`Files`).
    pub(crate) palette_scope: palette::PaletteScope,
    /// The palette's currently typed query - the same minimal hand-rolled append/backspace text
    /// field as [`Self::filter_query`] (see [`Self::handle_filter_key_down`]'s docs for why, over
    /// `vendor/zed/crates/gpui/examples/input.rs`'s full `EntityInputHandler`), with a real
    /// per-widget undo history attached (GitHub issue #17 - see [`text_history::TextField`]).
    /// Reset (text *and* history) on every `Self::open_palette`, which is a genuinely new widget
    /// instance, never on close.
    pub(crate) palette_query: text_history::TextField,
    /// The palette's currently highlighted result row - an index into
    /// `Self::build_palette_groups`' flattened (`crate::palette::state::flatten`) row order, moved by
    /// arrow keys and run by Enter.
    pub(crate) palette_selected: usize,
    pub(crate) palette_focus_handle: FocusHandle,
    /// Pre-open focus target and active session for [`Self::palette_focus_handle`] - see
    /// [`OverlayFocus`]/[`restore_focus`].
    pub(crate) palette_focus: OverlayFocus,
    /// The palette's file-candidate list (`crate::palette::state::FileCandidate`, one per non-directory
    /// [`Self::file_tree`] entry, up to `Settings.file_tree.max_entries`) - built once by
    /// [`Self::rebuild_palette_file_candidates`] when `file_tree`/the diff reload, not rebuilt on
    /// every `Self::build_palette_groups` call (which runs on every render while the palette is
    /// open, up to ~30x/sec during a streaming session). Session/command candidates aren't
    /// cached the same way: they're few, and a session's status dot is genuinely live per-render
    /// data with no stable invalidation point.
    pub(crate) palette_file_candidates: Vec<palette::FileCandidate>,
    /// The session rail's user-adjustable width (240-340px), dragged via the resize handle on
    /// the rail's right edge (see [`Self::apply_pane_resize`]/`crate::root::layout::rail_width_for_cursor`).
    pub(crate) rail_width: Pixels,
    /// The files/changes panel's user-adjustable width - see [`Self::rail_width`]'s docs,
    /// mirrored on the panel's left edge (`crate::root::layout::panel_width_for_cursor`).
    pub(crate) panel_width: Pixels,
    /// The window body's current paint bounds - captured every render by a `gpui::canvas` child
    /// (see [`Self::render`]'s body child list) and read by [`Self::apply_pane_resize`] to turn
    /// a drag's cursor position into a pane width, the same pattern
    /// `vendor/zed/crates/workspace/src/workspace.rs`'s own `bounds` field uses for its dock
    /// resize. `Bounds::default()` until the first paint; harmless since nothing reads it before
    /// a resize handle can be dragged.
    pub(crate) body_bounds: gpui::Bounds<Pixels>,
    /// Armed by a left mouse-down on the title bar's drag area, consumed by the next mouse-move
    /// to call `Window::start_window_move` - see [`Self::render_title_bar`]'s docs for why this
    /// two-step dance (verified against `vendor/zed/crates/platform_title_bar/src/
    /// platform_title_bar.rs`'s same pattern) is needed instead of starting the move on
    /// mouse-down directly.
    pub(crate) title_bar_move_armed: bool,
    /// The session rail's grouping mode (`by urgency` / `by project`). See
    /// [`crate::rail::state::RailMode`].
    pub(crate) rail_mode: RailMode,
    /// The rail's filter query - filters the rendered session/worktree rows in both grouping
    /// modes (see `crate::rail::state::filter_sessions`/`filter_worktree_rows`). Carries a real
    /// per-widget undo history (GitHub issue #17 - see [`text_history::TextField`]); unlike the
    /// palette's, this widget lives for the whole session, so its history does too.
    pub(crate) filter_query: text_history::TextField,
    pub(crate) filter_focus_handle: FocusHandle,
    /// The rail's *root container*'s focus handle - the app's real "nowhere else to put focus"
    /// fallback target (`Self::select_worktree`, `Self::close_session`, `Self::cancel_new_file`),
    /// deliberately **not** [`Self::filter_focus_handle`].
    ///
    /// Those three sites used to fall back onto the filter field itself, which an adversarial
    /// audit found had become a real, reachable bug once GitHub issue #17 tagged that field
    /// `"text-input"`: closing the last session focused a text input the user never asked to type
    /// in, and `Ctrl+Z` there resolved to `TextUndo` against an empty field - a silently swallowed
    /// keystroke with no feedback, instead of the worktree-level `Undo` that had always handled it
    /// (`crate::default_key_bindings` scopes that one `!terminal && !text-input`). The rail's root
    /// div carries no key context of its own, so focusing *it* keeps the focused `FocusId`
    /// genuinely findable in the next rendered frame - the actual invariant the fallback exists to
    /// protect - without claiming to be a text widget.
    pub(crate) rail_focus_handle: FocusHandle,
    /// Real `+N -M`/has-changes totals per worktree or session cwd, refreshed by the
    /// periodic background task started in `Self::new` - see `crate::rail::state::
    /// compute_status_snapshot`'s docs. Read (never written outside that task's completion
    /// callback) by `Self::build_session_rows` each render.
    pub(crate) diff_cache: HashMap<PathBuf, rail::DiffSummary>,
    /// Real clean/merged notes per worktree path, from the same periodic refresh as
    /// [`Self::diff_cache`] - powers "by project" mode's session-less worktree rows and the
    /// rail footer's `prune` action.
    pub(crate) worktree_notes: HashMap<PathBuf, rail::WorktreeNote>,
    /// Real `wt_core::diff::AheadBehind` counts per worktree/session cwd, from the same
    /// periodic refresh as [`Self::diff_cache`] - the status bar's `↑2 ↓0` indicator for the
    /// active session's worktree.
    pub(crate) ahead_behind_cache: HashMap<PathBuf, wt_core::diff::AheadBehind>,
    /// Real, live per-pid CPU%/memory samples for every currently open session's process
    /// (`crate::status_bar::process_stats`), refreshed by the same periodic background task as
    /// [`Self::diff_cache`] - see `Self::start_status_polling`'s docs for why this rides the
    /// same timer rather than a second, independent polling loop. Keyed by OS pid; an entry is
    /// absent for a pid not yet sampled (or already exited).
    pub(crate) process_stats: HashMap<u32, process_stats::ProcessSample>,
    /// Real, bounded disk-usage total across every listed worktree (see
    /// `crate::rail::state::disk_usage_bytes`'s docs for the real `std::fs` walk and its cap),
    /// recomputed whenever the worktree list reloads. `None` while the very first
    /// computation is still in flight.
    pub(crate) disk_usage: Option<(u64, bool)>,
    /// Per-worktree half of the same computation [`Self::disk_usage`] sums
    /// (`crate::rail::state::disk_usage_bytes(path)`, see [`Self::load_disk_usage`]) - kept as its own
    /// field because the Settings › Worktrees page needs a per-row size, not just the rail
    /// footer's aggregate.
    pub(crate) worktree_disk_usage: HashMap<PathBuf, (u64, bool)>,
    /// Feedback from the most recent `prune` click (how many worktrees were removed, or an
    /// error), shown in the rail footer until the next prune attempt or worktree reload.
    pub(crate) prune_status: Option<String>,
    /// `true` after one click on the footer `prune` button, cleared after the confirming click
    /// or by any other rail interaction in the meantime - see [`Self::request_prune`]'s docs for
    /// why prune is a two-click confirmation.
    pub(crate) prune_confirm_armed: bool,
    /// `true` for the duration of an in-flight [`Self::execute_prune`] batch - guards against a
    /// second confirming click spawning a second, racing batch that would overwrite
    /// [`Self::_prune_task`] and drop (cancel) the first mid-flight. Set synchronously before
    /// spawning, reset in that same task's completion handler.
    pub(crate) prune_in_flight: bool,
    /// The real command-pattern undo/redo stack (Revision R10,
    /// `crate::worktree_history::flow`) - see [`undo::UndoStack`]'s own docs for the cursor
    /// model. Only ever mutated from inside a background task's completion handler, after the
    /// real `wt_core::undo::*` call it corresponds to has actually succeeded - never
    /// speculatively at click time.
    pub(crate) undo_stack: undo::UndoStack,
    /// `Some(kind)` for the duration of any in-flight "keep all changes"/"discard worktree"/
    /// `Undo`/`Redo` operation, naming *which* one - not just a bare `bool` - so
    /// `Self::render_pty_footer`'s busy label ("keeping…"/"discarding…") can honestly reflect
    /// what's actually running instead of guessing from which button happens to be visible (a
    /// real, live-reproduced bug an audit caught: undoing a "keep all changes" made every
    /// visible `Discard worktree` button across every session read "discarding…"). A single
    /// field shared across all four, not four independent guards or a generation counter: these
    /// are the only operations that ever mutate real git history or a worktree's own existence
    /// for this feature, so fully serializing them (a second click of *any* of the four while
    /// one is in flight is a no-op, mirroring [`Self::prune_in_flight`]'s own
    /// single-flag-per-feature precedent) is sufficient, on its own, to make "a slow undo/redo
    /// op racing a newer one" structurally impossible - there can never be a second one in
    /// flight to race with. See `crate::worktree_history::flow`'s own module docs for why this
    /// is a deliberate simplification of - not a skip of - this project's usual
    /// task-slot/generation-guard discipline.
    pub(crate) worktree_history_op_in_flight: Option<worktree_history::WorktreeHistoryOpKind>,
    /// Feedback from the most recent "keep all changes"/"discard worktree"/`Undo`/`Redo`
    /// operation, shown in the status bar
    /// (`status_bar::render::AdeApp::render_status_worktree_history_notice`) until the next one -
    /// deliberately its own render slot, independent of [`Self::prune_status`] (see that
    /// method's own docs for why sharing one slot with `prune_status` was a real bug: an
    /// unrelated prune click could permanently hide every future worktree-history status for the
    /// rest of the session).
    pub(crate) worktree_history_status: Option<String>,
    /// `Some(id)` after one click on session `id`'s "Discard worktree" footer button, cleared by
    /// most other gestures in the meantime (mirroring [`Self::prune_confirm_armed`]'s own "most
    /// other gestures disarm it" discipline, applied everywhere that field is - see
    /// `crate::worktree_history::flow::AdeApp::request_discard_worktree`'s own docs for why this
    /// destructive-feeling action gets the same two-click confirmation as prune, even though it's
    /// now genuinely undoable). Not a universal "any gesture at all clears it" guarantee, though:
    /// arming *this* field's own sibling ([`Self::prune_confirm_armed`]'s first, arming click)
    /// does not clear this one, and vice versa - only each field's own confirm/cancel/execute
    /// paths, and a handful of other real navigation gestures, clear it.
    pub(crate) discard_confirm_armed: Option<SessionId>,
    /// Whether the Settings surface is currently replacing the three-zone body - see
    /// [`Self::open_settings`]/[`Self::close_settings`], which use the same
    /// capture-and-restore shape as [`Self::palette_open`].
    pub(crate) settings_open: bool,
    /// The Settings nav column's real overlay scrollbar handle (GitHub issue #30) - a plain
    /// `gpui::ScrollHandle`, not `UniformListScrollHandle`: the nav groups render eagerly (there
    /// are only ever a handful of them), not through a `uniform_list`.
    pub(crate) settings_nav_scroll_handle: gpui::ScrollHandle,
    /// The Settings content column's own equivalent of [`Self::settings_nav_scroll_handle`] - a
    /// separate handle since the two columns scroll independently of each other.
    pub(crate) settings_content_scroll_handle: gpui::ScrollHandle,
    /// Which Settings nav page is selected - persists across opens/closes (unlike the palette's
    /// query/scope, which resets every time).
    pub(crate) settings_page: settings::SettingsPage,
    pub(crate) settings_focus_handle: FocusHandle,
    /// Pre-open focus target for [`Self::settings_focus_handle`] - see [`OverlayFocus`].
    pub(crate) settings_focus: OverlayFocus,
    /// The Settings › Agents page's rows (`crate::settings::state::detect_agent_rows`, via
    /// `pty_core::resolve_on_path`), computed off the foreground thread and cached here (see
    /// [`Self::load_agent_rows`]) rather than recomputed inline - a `$PATH` search for a
    /// not-found binary measures ~30ms, which would cap the whole Settings surface's frame rate
    /// if run inline. `Vec::new()` until the first load completes.
    pub(crate) agent_rows: Vec<settings::AgentRow>,
    /// The context bar's `Merge` action and Surface D's conflict-resolution flow - see
    /// [`crate::merge::state::MergeFlow`]'s docs. `None` when no session has an in-flight merge or
    /// unresolved conflict.
    pub(crate) merge_flow: Option<merge::MergeFlow>,
    /// `true` for the duration of an in-flight `Complete merge`/`Abort merge` git operation -
    /// guards against a fast Abort-after-Complete double-click letting `git merge --abort` race
    /// an in-flight `git commit` and discard already-resolved conflict work.
    pub(crate) merge_op_in_flight: bool,
    /// Cached per-side syntax highlighting for whichever conflict hunk is currently active in
    /// [`Self::merge_flow`] - recomputed only at the real points that can change (`start_merge`
    /// finding a `Conflicted` state, `resolve_active_hunk` advancing to the next hunk), never
    /// from `render()`. Keyed on `(relative_path, ConflictHunk)`, not the hunk alone: two
    /// different conflicted files can have byte-identical hunk content (same lines/labels/start
    /// lines) but different extensions, which would otherwise incorrectly reuse one file's
    /// highlighting for the other - `ConflictHunk` already derives `PartialEq`, `PathBuf`'s own
    /// is a plain path compare.
    pub(crate) merge_highlight_cache: Option<(
        PathBuf,
        ConflictHunk,
        Vec<code_view::RenderedLine>,
        Vec<code_view::RenderedLine>,
    )>,
    /// Real, dedicated hand-edit state for Surface D's conflict-resolution flow (Revision
    /// R8.5c) - see [`merge::MergeEditState`]'s own docs for why this is separate from
    /// [`Self::edit_buffers`]. `None` whenever no hand-edit is in progress, including while a
    /// merge conflict is showing but the user hasn't toggled hand-edit mode on for the active
    /// file. Torn down (`crate::merge::flow::AdeApp::clear_merge_edit_state`) at every real
    /// merge-flow-ending point (abort/complete/dismiss/session-close) and by a fresh
    /// [`Self::start_merge`], and whenever the flow's own active file (matched by path) advances
    /// past whatever file this hand-edit is for.
    pub(crate) merge_edit: Option<merge::MergeEditState>,
    /// Focus target for the merge hand-edit whole-file editor's outer container
    /// (`crate::merge::editing::render_merge_edit_view`) - `track_focus`'d there, the same
    /// "must be the exact focused node the real key-context/`on_action` dispatch walks up from"
    /// discipline [`Self::code_focus_handle`] already establishes (see
    /// `crate::code_surface::render::AdeApp::render_code_surface`'s own docs for the real,
    /// live-reproduced bug getting that wrong once already caused).
    pub(crate) merge_edit_focus_handle: FocusHandle,
    pub(crate) merge_edit_scroll_handle: UniformListScrollHandle,
    /// The merge hand-edit whole-file view's own, dedicated equivalent of
    /// [`Self::file_view_row_layout`] - deliberately never shared with the File view's own map,
    /// so the two virtualized lists' click/cursor hit-testing caches can never cross-contaminate
    /// (the exact class of bug this project's own audits - e.g. BUILD-LOG's Revision R9a
    /// diff-highlight-cache finding - keep finding when two independent surfaces share one
    /// cache).
    pub(crate) merge_edit_row_layout: HashMap<usize, (gpui::Bounds<Pixels>, gpui::ShapedLine)>,
    /// See [`Self::file_view_last_layout`]/[`Self::file_view_last_bounds`]/
    /// [`Self::file_view_last_layout_for`]'s own docs - the merge hand-edit view's own dedicated
    /// equivalents, read by the generalized `EntityInputHandler::bounds_for_range`/
    /// `character_index_for_point` impls when the merge buffer (not the File view's) is the
    /// active edit target.
    pub(crate) merge_edit_last_layout: Option<gpui::ShapedLine>,
    pub(crate) merge_edit_last_bounds: Option<gpui::Bounds<Pixels>>,
    pub(crate) merge_edit_last_layout_for: Option<(PathBuf, usize)>,
    /// Mirrors [`Self::file_save_pending`]/[`Self::file_save_running`]'s serial-writer-loop
    /// discipline (see `crate::merge::flow::AdeApp::save_merge_edit`'s own docs), scoped to
    /// the single [`Self::merge_edit`] slot rather than per-path - only one hand-edit buffer can
    /// ever exist at once.
    pub(crate) merge_edit_save_pending: bool,
    pub(crate) merge_edit_save_running: bool,
    /// The most recent hand-edit save's real failure, if any - surfaced next to the hand-edit
    /// editor's own Save button, mirroring [`Self::file_save_error`]'s convention. Cleared by
    /// the next successful save, or by leaving hand-edit mode
    /// (`crate::merge::flow::AdeApp::clear_merge_edit_state`).
    pub(crate) merge_edit_save_error: Option<String>,
    pub(crate) _merge_edit_save_task: Option<Task<()>>,
    /// A real, monotonically-increasing counter bumped by every [`Self::start_merge`] call - the
    /// source of [`merge::MergeFlow::generation`], mirroring [`Self::completions_generation`]'s
    /// own "stamp a background operation at dispatch time, compare it at completion time before
    /// applying a result" convention.
    pub(crate) merge_generation: u64,
    /// A real, monotonic counter bumped by every `crate::merge::flow::AdeApp::
    /// start_merge_hand_edit` call that actually seeds a *fresh* `EditBuffer` - the source of
    /// [`merge::MergeEditState::buffer_id`], mirroring [`Self::merge_generation`]'s own "stamp a
    /// background operation at dispatch time, compare it at completion time" convention, but at
    /// per-*buffer* granularity rather than per-merge-*attempt* granularity - see that field's
    /// own docs for the real race this closes that `merge_generation` alone cannot.
    pub(crate) merge_edit_buffer_id: u64,
    /// Test-only seam: an artificial delay [`crate::merge::flow::AdeApp::
    /// spawn_merge_edit_save_loop`] awaits (via the GPUI test clock) before each real write -
    /// mirrors [`Self::settings_save_test_delay`]'s own identical, established pattern for the
    /// same real reason: letting a test deterministically hold one save pending while it
    /// mutates [`Self::merge_edit`] underneath it, to really exercise the buffer-identity guard
    /// [`merge::MergeEditState::buffer_id`]'s own docs describe. `#[cfg(test)]`-gated end to
    /// end, so no test-only state exists in a production build.
    #[cfg(test)]
    pub(crate) merge_edit_save_test_delay: Option<Duration>,
    pub(crate) _load_worktrees_task: Option<Task<()>>,
    pub(crate) _load_file_tree_task: Option<Task<()>>,
    pub(crate) _load_diff_task: Option<Task<()>>,
    /// The in-flight `code_view::load_file` task for whichever path [`FileLoadState::Loading`]
    /// names - dropping it (a fresh assignment, or `Self::select_worktree`'s reset) cancels that
    /// load immediately, per GPUI's `Task`-drop-cancels semantics.
    pub(crate) _file_load_task: Option<Task<()>>,
    pub(crate) _status_poll_task: Option<Task<()>>,
    pub(crate) _disk_usage_task: Option<Task<()>>,
    pub(crate) _prune_task: Option<Task<()>>,
    /// The single in-flight "keep all changes"/"discard worktree"/`Undo`/`Redo` background task,
    /// guarded by [`Self::worktree_history_op_in_flight`] - see that field's own docs for why one
    /// slot shared across all four is sufficient discipline here.
    pub(crate) _worktree_history_task: Option<Task<()>>,
    pub(crate) _agent_rows_task: Option<Task<()>>,
    pub(crate) _merge_task: Option<Task<()>>,
    /// `Self::clear_merge_flow_for_closed_session`'s best-effort abort - kept separate from
    /// [`Self::_merge_task`] so a cleanup-triggered abort can never overwrite (and thus cancel)
    /// an in-flight `complete_merge_flow`/`abort_merge_flow` commit, which would strand
    /// [`Self::merge_op_in_flight`] at `true` and let `git merge --abort` race an in-flight
    /// `git commit`.
    pub(crate) _merge_cleanup_task: Option<Task<()>>,
    /// Every in-flight [`Self::resolve_active_hunk`] background write
    /// (`wt_core::merge::write_resolved_file`) - a [`TaskPool`], not a single slot, since
    /// resolving one file's hunk while a different file's write is still in flight must not
    /// cancel that earlier write (dropping a `Task` cancels it immediately) and leave real
    /// conflict markers on disk while the in-memory model reports it resolved.
    pub(crate) _merge_write_tasks: TaskPool,
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
    ///
    /// A language whose primary needs a coordinated companion process (see
    /// `crate::language::CompanionServer` - Vue is the one real case) gets a **second,
    /// independent entry** here, keyed by that companion's own distinct
    /// `CompanionServer::client_key` rather than its bare binary name. That's deliberate: the
    /// companion then goes through 100% of the same already-proven spawn/poll/evict machinery as
    /// any other client, and its distinct key means a Vue-flavored `typescript-language-server`
    /// (carrying an extra real plugin) can never collide with, or be silently reused as, the
    /// plain one a `.ts` file in the same repo spawns. `crate::lsp::client::LspConnection` is what
    /// presents a matched pair as one thing to callers.
    pub(crate) lsp_clients: HashMap<(PathBuf, &'static str), LspClientState>,
    /// Absolute paths that have already had `textDocument/didOpen` sent for their owning
    /// [`Self::lsp_clients`] entry - checked by [`Self::render_file_view`] so a re-render never
    /// re-sends `didOpen` with a stale version. Never removed on file close: this viewer
    /// deliberately doesn't send a matching `didClose` (see [`Self::dispatch_did_open`]).
    pub(crate) lsp_opened_files: HashSet<PathBuf>,
    /// Real, monotonically-increasing `textDocument/didChange` document versions (Revision
    /// R8.5b), keyed by absolute path - matching [`Self::lsp_opened_files`]'s own key convention,
    /// since both track per-*file* (not per-worktree-relative-path) LSP document identity. Seeded
    /// to `1` (matching [`Self::dispatch_did_open`]'s own hardcoded `didOpen` version) the first
    /// time a real sync is sent for a path, and bumped by [`Self::prepare_lsp_sync`] on every real
    /// `didChange` it plans to send - never on a tick that skips sending one (unchanged content,
    /// or no ready client), so a version is never "spent" without a matching real notification.
    pub(crate) lsp_document_versions: HashMap<PathBuf, i32>,
    /// The real buffer content last *successfully* sent via a `didChange` notification for each
    /// open, dirty file - keyed by worktree-relative path, matching [`Self::edit_buffers`]'s own
    /// convention (unlike [`Self::lsp_document_versions`] above, this only ever needs to answer
    /// "does the *editor's* buffer already match what the server was told", which is naturally
    /// worktree-relative-scoped the same way the buffer itself is). [`Self::prepare_lsp_sync`]'s
    /// real "is there anything new to sync" check compares against this rather than resending
    /// identical content (and burning a real document version) on every debounce tick even when
    /// nothing changed since the last one that actually fired. Written only from [`Self::
    /// schedule_lsp_sync`]'s async continuation, and only after a real `did_change_full` call has
    /// genuinely returned `Ok` (Revision R8.5b audit finding 6's fix) - never at *plan* time in
    /// [`Self::prepare_lsp_sync`], which would confidently record content as "sent" before the
    /// send was even attempted, let alone known to have succeeded.
    pub(crate) lsp_last_synced_content: HashMap<PathBuf, String>,
    /// The real document version (see [`Self::lsp_document_versions`]) whose content was most
    /// recently *successfully* sent via a real `didChange`, keyed the same worktree-relative way
    /// as [`Self::lsp_last_synced_content`] (written alongside it, same real "the send genuinely
    /// succeeded" moment - Revision R8.5b audit finding 6). Compared against [`Self::
    /// lsp_diagnostics_confirmed_version`] by [`Self::render_file_view`]'s own `sync_pending`
    /// banner to answer a stronger question than "was the content sent": "has the server actually
    /// *answered* for it yet".
    pub(crate) lsp_synced_version: HashMap<PathBuf, i32>,
    /// The highest real document version [`Self::schedule_lsp_sync`]'s diagnostics-pull sequence
    /// (or, for a server with no real pull support, the send itself) has *confirmed* an actual
    /// answer for - keyed the same worktree-relative way as [`Self::lsp_last_synced_content`]
    /// (Revision R8.5b audit findings 5/6). While this trails [`Self::lsp_synced_version`], the
    /// server genuinely has the latest edit but hasn't answered for it yet - the real gap
    /// [`Self::render_file_view`]'s own `sync_pending` banner now stays honestly "pending"
    /// through, not just through the `didChange` send itself. Written with `.max(..)`, never a
    /// bare overwrite, so a real, late-arriving confirmation for an older version (the same
    /// reordering [`lsp_core::LspClient::pull_diagnostics`]'s own version guard protects against
    /// for the diagnostics map itself) can never regress this back down.
    pub(crate) lsp_diagnostics_confirmed_version: HashMap<PathBuf, i32>,
    /// A real, per-absolute-path cache of [`lsp_core::LspClient::uri_for_path`]'s own result
    /// (Revision R8.5b audit finding 8's fix for a real hard-rule violation) - populated exactly
    /// once per path, off the GPUI foreground thread, by [`Self::dispatch_did_open`]'s own
    /// background task (the same real moment a path's content is first read for `didOpen`), and
    /// read (never recomputed inline) by [`Self::prepare_lsp_sync`] on every subsequent debounced
    /// sync tick. `uri_for_path` performs a real, blocking `canonicalize()` syscall - this
    /// codebase's own established convention (see [`Self::schedule_lsp_sync`]'s own docs on the
    /// identical rule already followed for `lsp_core::LspClient::diagnostics_for`) is that such a
    /// call is never acceptable to run inline on the GPUI thread; an earlier version of [`Self::
    /// prepare_lsp_sync`] did exactly that, on every single real debounce tick, before this cache
    /// existed. Pruned alongside [`Self::lsp_document_versions`] by [`Self::
    /// evict_stale_lsp_clients`]'s own root-scoped retain pass (same absolute-path-keyed
    /// convention, same reasoning for why a blanket per-worktree-switch reset isn't needed).
    pub(crate) lsp_uri_cache: HashMap<PathBuf, lsp_core::lsp_types::Uri>,
    /// Every in-flight debounced real `textDocument/didChange` sync (Revision R8.5b), one slot
    /// per worktree-relative path - see [`Self::schedule_lsp_sync`]'s own docs for why a single
    /// slot (not a [`TaskPool`]) is the real, correct discipline here: a fresh edit to the same
    /// path must cancel (not race alongside) whatever earlier sync cycle was still in flight for
    /// it, the same "only the most recent keystroke's work should ever land" guarantee
    /// [`Self::_rehighlight_tasks`] already establishes for re-highlighting - the real mechanism
    /// this project's own history (Revision R3, R5.5) keeps needing for exactly this "a fast
    /// typist must never produce out-of-order server state" shape. Also explicitly cleared by
    /// [`crate::code_surface::tabs::AdeApp::close_file_tab`] for whichever path's tab just closed
    /// (Revision R8.5b audit finding 3), so an in-flight sync for a file that's no longer open
    /// can't keep running.
    pub(crate) _lsp_sync_tasks: HashMap<PathBuf, Task<()>>,
    /// The single, real in-flight `textDocument/completion` request task, if any (Revision
    /// R8.5b audit finding 2) - a single slot, not a [`TaskPool`], mirroring [`Self::
    /// _hover_request_task`]'s own reasoning: [`Self::completions`] shows only one popup at a
    /// time, so a fresh completion request always supersedes an in-flight one. Deliberately its
    /// *own* slot, independent of [`Self::_lsp_sync_tasks`] - an earlier version awaited the
    /// completion request inline, at the end of the same task that also ran the diagnostics-pull
    /// retry sequence, which meant a real completion response could never arrive until that whole
    /// sequence (up to a real, measured ~8s) finished. [`Self::completions_generation`]'s own
    /// staleness check still independently guards against a stale result ever being applied, the
    /// same defense-in-depth this module already establishes elsewhere.
    pub(crate) _completions_request_task: Option<Task<()>>,
    /// Surface C's real Completions popup state (Revision R8.5b) - `None` when no popup is
    /// showing. Keyed implicitly to whichever [`Self::edit_buffers`] path
    /// [`CompletionsEntry::path`] names; a stale entry for a file that's no
    /// longer open simply never matches [`Self::active_editable_path`] and is treated as absent
    /// by every render/keybinding site that reads it.
    pub(crate) completions: Option<CompletionsEntry>,
    /// A real generation counter bumped every time a completions request is dispatched or the
    /// popup is dismissed (`Self::dismiss_completions`) - see [`Self::schedule_lsp_sync`]'s own
    /// docs for the real, live race this closes: an in-flight `textDocument/completion` request
    /// whose *task* wasn't cancelled (e.g. the user pressed Escape, which doesn't touch
    /// [`Self::_completions_request_task`]) must not resurrect a popup the user already dismissed
    /// once its slow response finally arrives. A request's completion handler only ever applies
    /// its result if the generation it captured at dispatch time still matches this field.
    pub(crate) completions_generation: u64,
    /// Per-line diagnostic index (`crate::lsp::diagnostics::index_diagnostics_by_line`) for
    /// whichever Rust file [`Self::render_file_view`] last rendered - recomputed at the start of
    /// every render for a Rust file, cleared for a non-Rust file so diagnostics can't bleed
    /// across files.
    pub(crate) file_view_diagnostics: HashMap<usize, Vec<diagnostics_view::LineDiagnostic>>,
    /// The real error-severity diagnostic count for whichever file [`Self::render_file_view`]
    /// most recently rendered a `rust-analyzer` status for - exactly the same
    /// `lsp::LspFileStatus::Analyzed { errors, .. }` value `code_surface::file_view::render_file_status_bar`
    /// itself displays for that same file, in the same frame (set right alongside
    /// [`Self::file_view_diagnostics`], from the same `lsp_status` computation). `None` whenever
    /// that render didn't produce a real `Analyzed` status (non-Rust file, or the LSP client is
    /// still spawning/indexing/failed) - not a fabricated `Some(0)`.
    /// [`Self::status_bar_error_count`] reads this rather than re-deriving a count from
    /// [`Self::file_view_diagnostics`]'s own per-line index (whose per-line shape would
    /// over-count any diagnostic spanning multiple lines), so the two real error counts shown in
    /// the same frame - this one and the File view's own footer - can never disagree.
    pub(crate) file_view_error_count: Option<usize>,
    /// Every in-flight `lsp_core::LspClient::spawn`/`did_open` background task - a [`TaskPool`]
    /// for the same "independent operations" reason as [`Self::_merge_write_tasks`].
    pub(crate) _lsp_tasks: TaskPool,
    /// The single, long-lived background poll loop watching every ready [`Self::lsp_clients`]
    /// entry's wake channel and calling `cx.notify()` on a new `publishDiagnostics`. Started
    /// lazily (see [`Self::ensure_lsp_poll_task`]), then deliberately never reset to `None` -
    /// one poll loop serves however many clients exist.
    pub(crate) _lsp_poll_task: Option<Task<()>>,
    /// Surface C's hover-state cache - the outcome of the most recent click-triggered
    /// `textDocument/hover` request (see [`Self::request_hover`]), `None` before the first click
    /// or after switching files. Also doubles as [`Self::trigger_goto_definition`]'s target:
    /// there's no separately-tracked "symbol under consideration" in this read-only viewer.
    pub(crate) hover: Option<HoverEntry>,
    /// The single in-flight [`Self::request_hover`] background task, if any - a single slot
    /// (not a [`TaskPool`]) because hover requests are never independent: [`Self::hover`] shows
    /// only one entry at a time, so a new click always supersedes an in-flight one. Assigning a
    /// fresh task here drops the previous one immediately, closing the bug where rapid clicking
    /// during rust-analyzer's initial indexing (each hover request can block a worker thread for
    /// up to [`LSP_QUERY_TIMEOUT`]) let unbounded concurrent requests pin the shared executor.
    pub(crate) _hover_request_task: Option<Task<()>>,
    /// Every in-flight [`Self::trigger_goto_definition`] background task - a [`TaskPool`], unlike
    /// [`Self::_hover_request_task`]'s single slot, since F12 has no `still_current`
    /// short-circuit tying it to one live UI slot the way hover does.
    pub(crate) _goto_definition_tasks: TaskPool,
    /// One-shot "the next completed load of this exact file should land the cursor on this
    /// line, not line 1" instruction for [`Self::spawn_file_load`]'s completion handler, set by
    /// [`Self::navigate_to_definition`] when a go-to-definition result names a file that isn't
    /// already open. Keyed by the target path (not just a line number) so an unrelated file's
    /// completed load can never misapply a stale entry meant for a different, still-loading
    /// file. Consumed via `Option::take` (only when the path matches) by whichever of
    /// [`Self::render_file_view`] or `spawn_file_load`'s completion handler applies it first;
    /// explicitly cleared by [`Self::open_file_view`] on every fresh open and by a failed load.
    pub(crate) pending_cursor_line: Option<(PathBuf, usize)>,
    /// The config-file-backed settings struct - loaded once from `~/.config/jerry/
    /// settings.toml` at startup ([`Self::new`], via `Settings::load_or_init`) and re-saved
    /// ([`Self::persist_settings`]) on every change from the settings pages or the palette's
    /// `Window controls: …` entries. See `crate::settings::store`'s module docs for which fields
    /// are persisted-only vs. also applied.
    pub(crate) settings: Settings,
    /// The resolved path [`Self::persist_settings`] writes to - `Some(~/.config/jerry/
    /// settings.toml)` in production, `None` for every GPUI test (`Self::new_with_settings`),
    /// which makes a save a genuine no-op rather than a special-cased test skip.
    pub(crate) settings_path: Option<PathBuf>,
    /// The single in-flight [`Self::persist_settings`] serial-writer-loop task. Settings saves
    /// are never independent of each other (there is only one `settings.toml`), so this is a
    /// single slot rather than a [`TaskPool`]: a second edit while a save is running is picked
    /// up by the still-running loop (see [`Self::settings_save_pending`]/
    /// [`Self::settings_save_running`]), not raced against it with a second write. The loop
    /// always fully awaits one `save_at` call before checking for a newer edit, so two physical
    /// writes to the same path can never be concurrent - closing a real out-of-order-write bug a
    /// simpler "drop the previous task" approach could not, since dropping a `Task` cannot stop
    /// a disk write that has already started on a worker thread.
    pub(crate) _settings_save_task: Option<Task<()>>,
    /// `true` whenever there's a settings edit newer than the last write the serial writer loop
    /// started - a single flag, not a queue, since only the latest value at write time ever
    /// matters. Cleared only by the loop itself, in the same step that reads [`Self::settings`]
    /// fresh to write it.
    pub(crate) settings_save_pending: bool,
    /// `true` for as long as the serial writer loop ([`Self::_settings_save_task`]) is alive -
    /// guards [`Self::persist_settings`] against spawning a second loop while one is already
    /// draining [`Self::settings_save_pending`].
    pub(crate) settings_save_running: bool,
    /// Test-only seam: an artificial delay the serial writer loop awaits (via the GPUI test
    /// clock) before each `Settings::save_at` call, letting a test deterministically hold one
    /// edit's write pending while a later edit queues behind it. `#[cfg(test)]`-gated end to
    /// end, so no test-only state exists in a production build. Set via
    /// [`Self::set_settings_save_test_delay`].
    #[cfg(test)]
    pub(crate) settings_save_test_delay: Option<Duration>,
    /// The config banner's `TOML | JSON` display segment - not itself a [`Settings`] field; see
    /// `crate::settings::store`'s "TOML is the real file; JSON is a read-only alternate view"
    /// docs.
    pub(crate) settings_cfg_format: CfgFormat,
    /// The Settings › Language servers page's rows (`crate::settings::state::detect_lsp_rows`), cached
    /// the same way [`Self::agent_rows`] is.
    pub(crate) lsp_rows: Vec<settings::LspRow>,
    pub(crate) _lsp_rows_task: Option<Task<()>>,
    /// The Keybindings settings page's filter query - same minimal append/backspace shape as
    /// [`Self::filter_query`], and the same real per-widget undo history (GitHub issue #17).
    pub(crate) settings_keymap_filter: text_history::TextField,
    pub(crate) settings_keymap_filter_focus_handle: FocusHandle,
    /// The identity of the Keybindings row currently capturing a new chord, if any - see
    /// [`Self::start_recording_keybinding`]'s own docs for the real `App::intercept_keystrokes`
    /// mechanism this drives.
    pub(crate) keymap_recording: Option<keymap_overrides::BindingIdentity>,
    /// The live `App::intercept_keystrokes` subscription backing [`Self::keymap_recording`] -
    /// `Some` for exactly as long as a row is recording, dropped by
    /// [`Self::cancel_keybinding_recording`]/[`Self::finish_recording_keybinding`].
    pub(crate) _keymap_intercept: Option<Subscription>,
    /// A real, just-rejected rebind collision - `(identity of the row being recorded, message)` -
    /// shown inline under that row by [`Self::render_settings_keymap_page`] until the next
    /// recording attempt (successful or not) clears it.
    pub(crate) keymap_rebind_error: Option<(keymap_overrides::BindingIdentity, String)>,
    /// The live `Window::observe_window_appearance` subscription backing
    /// [`Self::sync_theme_to_system_appearance`] - held for the entity's whole lifetime, set up
    /// once at construction regardless of whether `Settings.theme.follow_system` starts on (see
    /// that method's own docs for why).
    pub(crate) _window_appearance_subscription: Subscription,
    /// Whether the tab strip's `+` menu popover is open - see [`Self::render_plus_menu`].
    /// Closed by its own scrim click, by picking a row, and defensively by
    /// [`Self::open_palette`]/[`Self::open_settings`] (it's rendered as an unconditional sibling
    /// of both, so it would otherwise paint over a surface it no longer makes sense above).
    pub(crate) plus_menu_open: bool,
    /// The tab strip's `+` button's painted bounds, captured every render (same `gpui::canvas`
    /// pattern as [`Self::body_bounds`]). [`Self::render_plus_menu`] positions the popover
    /// directly off this rather than a second, independently-computed offset that could drift
    /// once the rail's adjustable width shifts the button. `Bounds::default()` until first paint.
    pub(crate) plus_button_bounds: gpui::Bounds<Pixels>,
    /// Which of the Windows/Linux title bar's five menu labels ([`crate::title_bar::menu::TitleMenu::ALL`])
    /// has its real dropdown open right now, if any - see [`crate::title_bar::menu::render_title_menu`]'s own
    /// docs. Closed the same way [`Self::plus_menu_open`] is: its own scrim click, picking a row,
    /// and defensively by [`Self::open_palette`]/[`Self::open_settings`].
    pub(crate) title_menu_open: Option<title_bar::TitleMenu>,
    /// Each of the five title-bar menu labels' painted bounds, captured every render the same
    /// `gpui::canvas` way [`Self::plus_button_bounds`] is - indexed by
    /// [`crate::title_bar::menu::TitleMenu::index`]. [`crate::title_bar::menu::render_title_menu`] positions the open
    /// menu's popover directly off the matching entry. `Bounds::default()` until first paint.
    pub(crate) title_menu_button_bounds: [gpui::Bounds<Pixels>; title_bar::TitleMenu::ALL.len()],
    /// Every in-flight [`Self::new_agent_pane`] background `$PATH` detection - a [`TaskPool`]
    /// rather than a single slot, so two rapid "New agent pane" clicks each produce their own
    /// session instead of the second cancelling the first's still-in-flight search.
    pub(crate) _new_agent_pane_task: TaskPool,
    /// Real "New file" creation state (`crate::root::new_file`) - `Some` only while the inline
    /// name prompt is showing. See [`new_file::NewFileInputState`]'s own docs.
    pub(crate) new_file_input: Option<new_file::NewFileInputState>,
    pub(crate) new_file_focus_handle: FocusHandle,
    /// The most recent "New file" attempt's real refusal (name already exists, empty name, a
    /// path separator) - shown next to the inline prompt, mirroring [`Self::file_save_error`]'s
    /// convention. Cleared by [`Self::start_new_file`]/[`Self::create_new_file`]'s own success
    /// path.
    pub(crate) new_file_error: Option<String>,
    /// Each worktree's own real, drag-chosen tab order (GitHub issue #16) - session and file
    /// tabs interleaved, keyed by that worktree's cwd. Never itself the source of truth for
    /// which tabs exist (`Sessions`/[`Self::open_files`] still are - see
    /// [`Self::combined_tab_order`]'s own docs); only [`Self::reorder_tab`] writes to it, and a
    /// worktree with no entry here simply renders its sessions then its files, exactly the old
    /// two-block layout.
    pub(crate) tab_order: HashMap<PathBuf, Vec<work_surface::TabRef>>,
    /// The unified tab strip's real, precise drop-target indicator (GitHub issue #16's "better
    /// visual feedback" ask): `Some((target, insert_after))` while a tab is being dragged over
    /// `target`'s own tab, where `insert_after` says whether the cursor is over the right half
    /// of `target` (dragged tab would land immediately after it) or the left half (immediately
    /// before) - see [`Self::render_tab_strip`]'s per-tab `on_drag_move` wiring. Cleared on drop
    /// and, defensively, on any mouse-up over the workspace body, so a cancelled drag (released
    /// outside any drop target) can't leave a stale caret painted on a tab no drag is over
    /// anymore.
    pub(crate) tab_drag_insertion: Option<(work_surface::TabRef, bool)>,
    /// User-authored themes loaded from `~/.config/jerry/themes/*.toml` at construction time
    /// (GitHub issue #5) - real, additional `crate::settings::custom_theme::CustomTheme` entries
    /// layered on top of the six built-in `settings::THEME_DEFS`, not a replacement for them. See
    /// `crate::settings::custom_theme`'s own module docs for the file format, and
    /// `Self::apply_theme_selection`/`Self::render_settings_theme_page` for the two real
    /// consumers.
    pub(crate) custom_themes: Vec<custom_theme::CustomTheme>,
    /// Real, honestly-reported parse/validation failures from the last time `custom_themes` was
    /// (re)loaded - one entry per file that didn't make it in, shown on the Themes settings page
    /// rather than a bad hand-edit silently vanishing.
    pub(crate) custom_theme_load_errors: Vec<String>,
    /// The Themes page's most recent import/export/remove action result (`Ok` message or a real,
    /// honest `Err` one) - shown as an inline status line until the next action replaces it.
    pub(crate) custom_theme_status: Option<Result<String, String>>,
    /// The in-flight "Import theme..." real native file-picker task
    /// (`Self::start_import_custom_theme`) - a single slot, since only one file-open dialog can
    /// meaningfully be in flight at a time.
    pub(crate) _custom_theme_import_task: Option<Task<()>>,
    /// The in-flight "Export theme..." real native save-file-picker task
    /// (`Self::start_export_custom_theme`) - same one-slot reasoning as
    /// [`Self::_custom_theme_import_task`].
    pub(crate) _custom_theme_export_task: Option<Task<()>>,
    /// The in-flight, already-confirmed "Remove" background delete-and-reload task
    /// (`Self::execute_remove_custom_theme`) - same one-slot reasoning as
    /// [`Self::_custom_theme_import_task`].
    pub(crate) _custom_theme_remove_task: Option<Task<()>>,
    /// The in-flight "New from template" background write-and-reload task
    /// (`Self::start_create_theme_from_template`) - same one-slot reasoning as
    /// [`Self::_custom_theme_import_task`]; unlike Import there's no file-picker dialog to await
    /// first (the "file" is a fixed, embedded constant), but the actual disk write still runs on
    /// the background executor like every other real write in this module, so this still needs a
    /// slot to keep that task alive.
    pub(crate) _custom_theme_create_task: Option<Task<()>>,
    /// A real, just-armed "Remove" click on a custom theme card, by name - an adversarial audit
    /// caught the first version of this action deleting the user's file on a single click, unlike
    /// every other destructive action in this app (`Self::prune_confirm_armed`,
    /// `Self::discard_confirm_armed`, `Self::tree_delete_confirm`). `Self::request_remove_custom_theme`
    /// is the one real place this is armed/consumed - a first click on a given name arms it, a
    /// second click on the *same* name actually deletes. Disarmed by leaving the Themes settings
    /// page or reopening Settings (`Self::select_settings_page`/`Self::open_settings`), the same
    /// "most other gestures clear it" discipline `Self::discard_confirm_armed`'s own docs
    /// describe, scoped to this control's own page since nothing else in the app can arm it.
    pub(crate) custom_theme_remove_armed: Option<String>,
}

impl AdeApp {
    /// Single source of truth for which platform's title-bar variant/keycap glyphs render -
    /// [`Self::settings`]`.window.controls` is the persisted backing; the General settings page
    /// and the palette's `Window controls: …` entries both read/write it through this accessor
    /// and [`Self::set_window_controls_style`], never a second copy.
    pub(crate) fn window_controls_style(&self) -> WindowControlsStyle {
        self.settings.window.controls
    }

    /// Shared entry point for `Settings.appearance.interface_scale_percent` text scaling -
    /// scales only the text size passed to `.text_size(...)`, nothing else (padding/spacing/
    /// icon/fixed-chrome dimensions are out of scope).
    pub(crate) fn ui_text_size(&self, base_px: f32) -> Pixels {
        theme::ui_scale::scaled_px(base_px, self.settings.appearance.interface_scale_percent)
    }

    /// Sets [`Self::window_controls_style`] and persists it. The one write path both the
    /// General settings page and the palette's `Window controls: …` entries call, so they can
    /// never disagree about which override is active.
    pub(crate) fn set_window_controls_style(
        &mut self,
        style: WindowControlsStyle,
        cx: &mut Context<Self>,
    ) {
        self.settings.window.controls = style;
        self.persist_settings(cx);
        cx.notify();
    }

    /// Sets `Settings.blame.show_inline` and persists it - GitHub issue #29's own "user setting
    /// to hide it entirely" requirement. `crate::code_surface::blame_view::AdeApp::
    /// maybe_refresh_blame`/`current_line_blame` both check this field directly (not a cached
    /// copy), so flipping it off here also stops the real background `git blame` work, not just
    /// the rendering - a genuine off switch, not merely a visual one.
    pub(crate) fn set_show_inline_blame(&mut self, show: bool, cx: &mut Context<Self>) {
        self.settings.blame.show_inline = show;
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
    pub(crate) fn persist_settings(&mut self, cx: &mut Context<Self>) {
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

    /// Queues a background-executor save of [`Self::fold_state`] to [`Self::fold_state_path`].
    /// Called from every single expand/collapse - that immediacy is the point (GitHub issue #18
    /// §2 asks for fold changes to be "recorded immediately (crash-safe), not only on clean
    /// exit"), and `FoldState::save_at`'s write-temp-then-rename is what makes an interrupted
    /// write a no-op rather than a corrupted file.
    ///
    /// Structurally identical to [`Self::persist_settings`], including the "clear the running
    /// flag in the same synchronous step that decides to stop" trick that keeps a change landing
    /// at exactly the wrong moment from being silently dropped - see that method's own docs for
    /// the full reasoning. It is a genuine no-op with a `None` path (every GPUI test that hasn't
    /// asked for a real one).
    ///
    /// Unlike settings, the write is a *merge* (`FoldState::save_merged_at` against
    /// [`Self::fold_state_owned`]): a second `jerry` process browsing a different repository is
    /// writing the same file, and a whole-file write would erase its state.
    pub(crate) fn persist_fold_state(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.fold_state_path.clone() else {
            return;
        };
        self.fold_state_save_pending = true;
        if self.fold_state_save_running {
            return;
        }
        self.fold_state_save_running = true;
        let task = cx.spawn(async move |this, cx| {
            let mut attempt: u32 = 0;
            loop {
                // The state and the owned-key set are read in the *same* synchronous step, so the
                // pair handed to `save_merged_at` can never be a mix of two different moments.
                let step = this.update(cx, |this, _cx| {
                    if this.fold_state_save_pending {
                        this.fold_state_save_pending = false;
                        Some((this.fold_state.clone(), this.fold_state_owned.clone()))
                    } else {
                        this.fold_state_save_running = false;
                        None
                    }
                });
                let Ok(Some((state, owned))) = step else {
                    break;
                };
                let result = cx
                    .background_executor()
                    .spawn({
                        let path = path.clone();
                        async move { state.save_merged_at(&path, &owned) }
                    })
                    .await;
                match result {
                    Ok(()) => attempt = 0,
                    Err(err) => {
                        // Do **not** drop the change. `fold_state_save_pending` was cleared above,
                        // before the write, so without this a real failure (disk full, a read-only
                        // `~/.config`, a permissions change) would lose the user's expand/collapse
                        // with nothing but a log line - while this feature's whole claim is that a
                        // fold change is recorded immediately. Re-marking it pending means the very
                        // next iteration rewrites the *current* state, which is also why this needs
                        // no queue: only the latest value ever matters.
                        attempt += 1;
                        if attempt > FOLD_STATE_SAVE_MAX_ATTEMPTS {
                            log::error!(
                            "giving up saving {} after {FOLD_STATE_SAVE_MAX_ATTEMPTS} attempts \
                             ({err}) - file-tree fold state will not persist until something \
                             changes again",
                            path.display()
                        );
                            // Deliberately *not* re-marked pending: a permanently broken path would
                            // otherwise spin this loop forever. A later real expand/collapse calls
                            // `persist_fold_state` again and starts a fresh attempt budget, which is
                            // the right retry trigger - the user did something new.
                            continue;
                        }
                        log::warn!(
                        "failed to save {} (attempt {attempt}/{FOLD_STATE_SAVE_MAX_ATTEMPTS}): \
                         {err} - retrying",
                        path.display()
                    );
                        let requeued =
                            this.update(cx, |this, _cx| this.fold_state_save_pending = true);
                        if requeued.is_err() {
                            break;
                        }
                        // Linear backoff, so a transient failure (a full disk being cleared, a mount
                        // coming back) is retried promptly without hammering a broken path.
                        cx.background_executor()
                            .timer(FOLD_STATE_SAVE_RETRY_BACKOFF * attempt)
                            .await;
                    }
                }
            }
        });
        self._fold_state_save_task = Some(task);
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
            // A real, load-bearing baseline context tag (Revision R10) - not decorative. GPUI's
            // own `KeyBindingContextPredicate::eval_inner`
            // (`vendor/zed/crates/gpui/src/keymap/context.rs:277-280`) returns `false`
            // *immediately*, before even inspecting which predicate variant it is, whenever the
            // context stack for the current dispatch path is completely empty
            // (`contexts.last()` is `None`) - live-reproduced while wiring up `Undo`/`Redo`'s
            // `Some("!terminal")` scoping: with Settings open (a real focus target with no
            // `.key_context(..)` anywhere on its own ancestor chain), the stack was genuinely
            // empty, so `!terminal` never got a chance to evaluate "is 'terminal' absent" - it
            // just always returned `false` (never matching) regardless of whether a terminal
            // was anywhere in sight. `"app"` here guarantees the stack always has at least one
            // frame, so `!terminal` (and any future negated-context predicate) evaluates its
            // real, intended logic everywhere - confirmed by
            // `root::focus::tab_strip_keybinding_tests::
            // secondary_z_reaches_undo_once_real_focus_moves_off_the_terminal`.
            .key_context("app")
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
            .on_action(cx.listener(Self::handle_undo_action))
            .on_action(cx.listener(Self::handle_redo_action))
            .on_action(cx.listener(Self::handle_close_focused_tab_action))
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
            .when_some(self.title_menu_open, |el, menu| {
                el.child(self.render_title_menu(menu, cx))
            })
            .children(self.render_hover_card(cx))
            .children(self.render_completions_popover(cx))
            .when(self.new_file_input.is_some(), |el| {
                el.child(self.render_new_file_prompt(cx))
            })
            // The file tree's context menu and its delete confirmation (GitHub issue #19) -
            // both are window-positioned overlays, so they live here beside the `+` menu and the
            // "New file" prompt rather than inside the sidebar's own clipped column.
            //
            // Gated on `!settings_open`, which is a real fix rather than defensive padding
            // (found in this change's own review): the Settings surface *replaces* the workspace
            // body one child up, so an ungated menu would paint a full-window transparent scrim
            // and a file-tree menu over Settings, swallowing every click on the page underneath.
            // `Self::open_settings` clears `plus_menu_open`/`title_menu_open`/`new_file_input`
            // for exactly this reason; the tree's own state is cleared alongside them there, and
            // this guard is the belt to that braces. The context menu additionally requires the
            // Files tab, since every one of its actions targets a row only that tab renders.
            .when(
                !self.settings_open
                    && self.right_sidebar_view == RightSidebarView::Files
                    && self.tree_context_menu.is_some(),
                |el| el.child(self.render_tree_context_menu(cx)),
            )
            // The delete confirmation deliberately does *not* require the Files tab: it is a
            // window-level modal the user is mid-way through answering, not a tree affordance,
            // and hiding it on a tab switch would leave a destructive confirmation armed with no
            // way to answer or cancel it.
            .when(
                !self.settings_open && self.tree_delete_confirm.is_some(),
                |el| el.child(self.render_tree_delete_confirm(cx)),
            )
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
            // Defensive cleanup for `Self::tab_drag_insertion`: unlike `PaneResizeDrag` above,
            // this one *does* need an explicit mouse-up handler, because it isn't derived fresh
            // from the cursor position on every tick - it's the last tab strip
            // `on_drag_move::<DraggedTab>` claimed, which stays stale if the drag ends by
            // releasing outside any tab's own hitbox (a cancelled drag) rather than through a
            // real `on_drop`. The body spans virtually the whole window below the title bar, so
            // this reaches almost every real release point.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &gpui::MouseUpEvent, _window, cx| {
                    if this.tab_drag_insertion.take().is_some() {
                        cx.notify();
                    }
                }),
            )
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
pub(crate) struct OverlayFocus {
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
    pub(crate) fn capture(&mut self, window: &Window, sessions: &Sessions, cx: &App) {
        self.return_focus = window.focused(cx);
        self.opened_session = sessions.active_id();
    }

    /// Discards captured state without restoring it. Three real callers: [`AdeApp::close_palette`]'s
    /// Settings-showing-underneath branch and [`AdeApp::close_palette_keeping_result_focus`],
    /// both of which put focus somewhere real themselves instead of going through
    /// [`restore_focus`], and `crate::work_surface::render`'s session teardown.
    pub(crate) fn clear(&mut self) {
        self.return_focus = None;
        self.opened_session = None;
    }

    /// Forgets a captured target that is about to stop being rendered, leaving [`restore_focus`]
    /// to fall back to the active session's pane instead of focusing a node GPUI can no longer
    /// find in the frame.
    ///
    /// This exists for a real, reproduced case (found by the `tree-focus-bugfixes` branch's own
    /// adversarial audit): with the file tree focused, opening the palette captures
    /// `tree_focus_handle`; running the palette's own "Toggle Files / Changes" then unrenders the
    /// whole tree, and closing the palette restored focus straight onto it.
    /// `crate::sidebar::render::AdeApp::set_right_sidebar_view` already had a
    /// `tree_focus_handle.is_focused(window)` guard for the *direct* version of this, but the
    /// palette is what holds focus at that moment, so the guard could not see it. Every overlay
    /// that could be holding the tree's handle is swept there instead.
    pub(crate) fn forget_target(&mut self, handle: &FocusHandle) {
        if self.return_focus.as_ref() == Some(handle) {
            self.return_focus = None;
        }
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
pub(crate) fn restore_focus(
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

pub(crate) mod caret_blink;
pub(crate) mod focus;
pub mod layout;
pub(crate) mod new_file;
pub(crate) mod rem_scope;
pub(crate) mod resize;
pub(crate) mod scrollbar;
pub(crate) mod scrollbar_geometry;
pub(crate) mod state;
pub(crate) mod task_pool;
pub(crate) mod widgets;
