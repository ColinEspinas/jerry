//! The top-level three-pane window: a left worktree sidebar, a tabbed center pane of real
//! terminal sessions, and a right file tree, composed as GPUI entities.
//!
//! ## Offloading `wt-core`'s blocking calls
//!
//! `wt_core::list_worktrees` performs blocking I/O (its own docs say so explicitly: `gix`
//! object-database reads plus, for some paths, spawning `git`). It is never called directly
//! from `render` or from an event handler; instead [`AdeApp::load_worktrees`] hands it to
//! `cx.background_executor().spawn(..)` (GPUI's background thread pool) and only touches
//! `self` again inside a `this.update(cx, ..)` callback running back on the foreground
//! thread, once the background task's `Task` resolves. The same pattern is used for
//! `crate::file_tree::build_file_tree`, which does its own (smaller, but still real)
//! blocking `std::fs::read_dir` walk.
//!
//! ## Sessions/tabs, and what selecting a worktree does (and doesn't) do
//!
//! Step 3 had exactly one always-live terminal, respawned in place whenever the selected
//! worktree changed. Step 4 replaces that with [`crate::sessions::Sessions`]: any number of
//! independent, simultaneously-running terminal sessions (a plain shell, or a real agent CLI
//! like `claude`), each pinned to the worktree it was started in, shown as tabs in the
//! center pane.
//!
//! A deliberate behavior change from step 3 falls out of that: **selecting a worktree in
//! the sidebar no longer respawns anything.** It only updates [`AdeApp::selected`] (which
//! drives the file tree on the right, and which worktree `active_session_cwd` resolves to
//! for the *next* "New Shell"/"New Claude Session" click). Keeping step 3's
//! respawn-on-select behavior here would mean clicking a worktree in the sidebar - just to
//! browse its files - could silently kill a live agent session running in whatever tab
//! happened to be active. Spawning a new session is now its own explicit action (the
//! toolbar buttons), never an implicit side effect of browsing.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use gpui::{
    actions, div, font, prelude::*, px, uniform_list, App, BoxShadow, ClickEvent, Context,
    DragMoveEvent, Empty, FocusHandle, Focusable, KeyDownEvent, MouseButton, MouseDownEvent,
    Pixels, ScrollStrategy, Task, UniformListScrollHandle, Window, WindowControlArea,
};
use wt_core::diff::{DiffBase, DiffFile, DiffLineKind, FileChangeStatus, WorktreeDiff};
use wt_core::merge::{ConflictHunk, ConflictSegment, ConflictedPath};

use crate::changes::{self, ChangeTag};
use crate::code_view;
use crate::diagnostics_view;
use crate::file_tree::{self, FileTreeEntry, LangChip};
use crate::hover_view;
use crate::keymap::{self, WindowControlsStyle};
use crate::layout;
use crate::merge;
use crate::palette;
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

// The rail header's `+` / ⌘N control spawns a new session. Bound as a real GPUI action/
// keybinding (see `crate::run`'s `cx.bind_keys` call, and `Self::render` registering
// `.on_action(cx.listener(Self::handle_new_session_action))`) - verified against the real
// `actions!`/`KeyBinding` pattern `vendor/zed/crates/gpui/examples/input.rs` uses.
//
// Judgment call, documented rather than silently assumed: whether this fires while a
// terminal tab has keyboard focus was not exhaustively verified against GPUI's key-dispatch
// priority between bound actions and a focused element's own `.on_key_down` (see
// `crate::terminal_pane`'s key handler, which calls `cx.stop_propagation()` on every key it
// recognizes). It was confirmed to fire with the rail's own filter field focused or nothing
// focused. Going further (e.g. instrumenting GPUI's dispatch tree) was judged out of scope
// for this step; the rail's own `+` button is a real, always-available fallback either way.
//
// `TogglePalette` (⌘K) follows the exact same pattern - see
// `Self::handle_toggle_palette_action` and `crate::lib::run`'s matching `cx.bind_keys` entry.
// The same focus-priority caveat applies equally to it; not re-verified separately here.
//
// `ToggleSettings` (⌘,) follows the exact same pattern again - see
// `Self::handle_toggle_settings_action` and `crate::lib::run`'s matching `cx.bind_keys` entry.
// The literal keystroke string `"secondary-,"` was verified against two real precedents:
// `vendor/zed/assets/keymaps/default-macos.json` binds Zed's own `zed::OpenSettings` action to
// `"cmd-,"` (and its Linux keymap the `ctrl-,` equivalent), confirming GPUI's real keystroke
// parser accepts a bare `,` as a keystroke's key component; and
// `vendor/zed/crates/gpui/src/platform/keystroke.rs:143-150` confirms `"secondary"` is a real,
// separately-recognized modifier alias that resolves to exactly those two per-OS modifiers at
// compile time (`platform`/Cmd on macOS, `control` elsewhere) - see `crate::default_key_bindings`'s
// own docs for the real, live-reproduced bug that made `"cmd-,"` the wrong choice here (it always
// means the Super/Windows key on Linux/Windows, never Ctrl, regardless of what `crate::keymap`'s
// rendering shows).
//
// `GotoDefinition` (`F12`) follows the exact same pattern once more - see
// `Self::handle_goto_definition_action` and `crate::lib::run`'s matching `cx.bind_keys` entry
// for the literal `"f12"` keystroke's own real precedent. It reads `AdeApp::hover` (the File
// view's Hover-state cache, keyed by whichever real, real-file symbol was last clicked - see
// that field's own docs) rather than any separately-tracked "definition target": there is no
// other real notion of "the symbol under consideration" in this read-only viewer, and requiring
// a hover to have already resolved before `F12` can navigate is itself an honest constraint (the
// same real symbol/position a hover request would use is exactly what a definition request
// needs) rather than an arbitrary one. A real no-op (not an error, not a fabricated navigation)
// when nothing has been clicked yet, or the click wasn't on a real `.rs` file.
actions!(
    app,
    [NewSession, TogglePalette, ToggleSettings, GotoDefinition]
);

/// How often the rail's real background status refresh (real `wt_core::diff::
/// diff_against_base` and `wt_core::is_dirty`/`merge_status_against_base` calls, via
/// `crate::rail::compute_status_snapshot`) re-runs. Coarser than `crate::terminal_pane`'s
/// 33ms output-drain poll: those are cheap channel `try_recv`s, while this tick spawns real
/// `git` child processes and reads the object database per distinct worktree/session path -
/// frequent enough that the rail's status/diff numbers feel live, not so frequent that a
/// handful of open sessions turns into a constant stream of `git` spawns.
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// How often [`AdeApp::render_file_view`] is willing to actually call `std::fs::metadata` for
/// its file-freshness check, throttling an otherwise-unconditional-per-render blocking syscall
/// on the GPUI foreground thread - see [`AdeApp::file_view_last_freshness_check`]'s docs for the
/// real per-frame cost this avoids. Short enough that a file changed on disk (e.g. by an agent
/// mid-edit) is still picked up promptly; long enough that repeated renders of an unchanged,
/// already-open file (as frequent as every ~33ms during a streaming agent session) don't each
/// pay for their own `stat()`.
const FILE_FRESHNESS_CHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Cap on how many changed files the diff view turns into rendered elements, independent of
/// `wt_core::diff`'s own `MAX_FILES` cap (300) on the *loaded* diff. Mirrors
/// `file_tree::MAX_RENDERED_FILE_ENTRIES` for the same reason: `wt_core::diff` can hand back up
/// to 300 files, each carrying its own hunk lines on top, and laying all of that out as GPUI
/// divs on every render is the same kind of foreground-executor stall documented at
/// `file_tree::MAX_RENDERED_FILE_ENTRIES`'s use site, just with a much larger multiplier.
const MAX_RENDERED_DIFF_FILES: usize = 40;

/// Cap on how many hunk lines a single file's diff renders, independent of `wt_core::diff`'s
/// own per-file `MAX_HUNK_LINES_PER_FILE` cap (2000) on loaded data. Same reasoning as
/// `MAX_RENDERED_DIFF_FILES`: a single enormous file (e.g. a generated lockfile that slipped
/// past the loaded-data cap) shouldn't be allowed to blow up render time on its own.
const MAX_RENDERED_DIFF_LINES_PER_FILE: usize = 300;

/// How often [`AdeApp::ensure_lsp_poll_task`]'s background loop checks every ready
/// `lsp_core::LspClient` for a real, newly-arrived `publishDiagnostics` notification. Mirrors
/// `crate::terminal_pane::POLL_INTERVAL` (33ms) in shape but is deliberately coarser: pty output
/// is latency-sensitive (a human is actively typing/reading a live terminal), while LSP
/// diagnostics arrive on rust-analyzer's own multi-hundred-millisecond-to-many-second analysis
/// timeline (see `lsp_core::client`'s own docs) - polling every 33ms would burn CPU on `try_recv`
/// calls for no perceptible improvement in how fresh the File view's diagnostics look.
const LSP_DIAGNOSTICS_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// How long a real `textDocument/hover`/`textDocument/definition` request
/// ([`AdeApp::request_hover`]/[`AdeApp::trigger_goto_definition`]) waits for `rust-analyzer`'s
/// real response before giving up. Both requests run against an already-`Ready`
/// [`LspClientState`] (a real, already-initialized, already-indexing-or-indexed client - see that
/// enum's own docs), so this is a budget for one real query's own round trip, not for indexing
/// from a cold start the way `lsp_core::LspClient::spawn`'s own internal initialize timeout is -
/// shorter accordingly.
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
    /// Real, UI-only per-file "reviewed" toggle state for the Changes list
    /// (`design_handoff_jerry_ade/README.md`'s Zone 3 review checkboxes) - a file's path is in
    /// this set iff its checkbox is checked. There is no backend "review" concept yet (see the
    /// task brief this phase shipped against); this is a real, live, per-file `HashSet` toggle,
    /// not decoration - `Self::render_changes_header`'s progress bar and `N reviewed` count are
    /// both computed directly from its real membership.
    reviewed_files: HashSet<PathBuf>,
    /// The file whose real diff is currently opened in the centre pane (`design_handoff_
    /// jerry_ade/README.md`'s `open_change` state field), set by clicking a Changes row.
    /// `render_center_pane` shows this file's diff instead of the active session's terminal
    /// while it's `Some` - see that method's docs for the judgment call on how far this stands
    /// in for the design's full Surface C.
    open_change: Option<PathBuf>,
    /// The real, already-found-and-cloned `DiffFile` for whichever path [`Self::open_change`]
    /// currently names (`None` if that file has no real diff, or nothing is open) - kept up to
    /// date by [`Self::refresh_open_diff_file_cache`], the one real point where either of its two
    /// inputs (`open_change` itself, or [`Self::current_diff`]'s underlying diff) can change.
    /// `Self::render_center_pane` used to re-run this exact find-and-clone of the whole
    /// `DiffFile` (every one of its hunks, up to `wt_core::diff`'s own 2000-line-per-file cap)
    /// on *every single render* of the centre pane, just to hand it to
    /// `Self::render_code_surface`; this cache exists so that only happens when there's an
    /// actual reason to (a different file opened, or the diff itself reloading), not every
    /// frame. `Self::render_center_pane` moves it out (`Option::take`) rather than cloning it
    /// again before calling `Self::render_code_surface` (which needs `&mut self`, so it can't
    /// hold a live `&DiffFile` borrow of `self` across that call) and moves it back afterward -
    /// an `O(1)` pointer swap, not a second deep clone.
    open_diff_file_cache: Option<DiffFile>,
    /// The real file-tree path last resolved from a palette file result that had no diff to
    /// open (`Self::open_palette_file_result`'s docs) - highlighted in `Self::render_file_tree_row`
    /// exactly like a Changes row's own `Self::open_change` selection highlight
    /// (`design_handoff_jerry_ade/README.md`'s Zone 3 "Selected row bg `#1a1e21`", previously
    /// unwired for the Files tree since Phase D never gave individual file rows a click handler
    /// of their own).
    selected_tree_path: Option<PathBuf>,
    /// Surface C's real `Diff | File` toggle state (`design_handoff_jerry_ade/README.md`'s
    /// `code_view` state field) for whichever file [`Self::open_change`] currently names - set to
    /// `Diff` by [`Self::open_change_diff`] (a Changes-row click) and to `File` by
    /// [`Self::open_file_view`] (a Files-tree row click), and read by [`Self::render_code_surface`]
    /// alongside a real "does this file even have a diff to show" check (a file with no diff is
    /// always shown as `File`, regardless of this field - see that method's docs).
    code_view: code_view::CodeView,
    /// The centre's real Diff/File Surface C focus target - `track_focus`'d by
    /// [`Self::render_code_surface`]'s own outer container, exactly mirroring
    /// [`Self::settings_focus_handle`]'s own role for the Settings surface. Without this,
    /// [`Self::open_change_diff`]/[`Self::open_file_view`] leave `Window::focus` pointing at
    /// whatever was focused before (typically the active session's terminal pane) - a real
    /// `FocusHandle` that [`Self::render_center_pane`] stops rendering the instant `open_change`
    /// becomes `Some` (its own early-return docs explain why). GPUI's `Window::dispatch_action`
    /// resolves a dispatched action against whichever node the currently focused `FocusId` maps
    /// to in the *last rendered frame* (`vendor/zed/crates/gpui/src/window.rs`'s own
    /// `focus_node_id_in_rendered_frame`), falling back to the dispatch tree's synthetic root
    /// node - which sits above every one of `Self::render`'s own `on_action` handlers - when that
    /// `FocusId` isn't found there. A dangling handle silently breaks every action (⌘K, ⌘,, F12)
    /// until the user manually clicks something to re-establish real focus - the exact same bug
    /// class [`Self::palette_return_focus`]/[`Self::settings_return_focus`]'s own docs describe,
    /// now fixed here the same way: [`Self::open_change_diff`]/[`Self::open_file_view`] move
    /// focus onto this handle, and [`Self::close_change_diff`] restores whatever
    /// [`Self::code_return_focus`] captured.
    code_focus_handle: FocusHandle,
    /// See [`Self::code_focus_handle`]'s own docs - the real, pre-open focus target
    /// [`Self::open_change_diff`]/[`Self::open_file_view`] found via `window.focused(cx)`,
    /// captured only the first time either transitions [`Self::open_change`] from `None` to
    /// `Some` (never overwritten by a *different* file opening while one is already showing, the
    /// same "only capture on a real open, not on every navigation" rule
    /// [`Self::palette_return_focus`]'s own docs establish). `None` if nothing was focused yet
    /// (a completely fresh window). Restored (and cleared) by [`Self::close_change_diff`].
    code_return_focus: Option<FocusHandle>,
    /// See [`Self::palette_opened_session`]'s own docs for the exact real bug this mirrors and
    /// fixes, applied to the code/file Surface C instead of the palette overlay: which session
    /// was active when [`Self::code_return_focus`] was captured, so [`Self::close_change_diff`]
    /// can tell whether restoring it is still safe (the active session may have changed while the
    /// surface was open).
    code_opened_session: Option<SessionId>,
    /// The File view's real `gpui::uniform_list` scroll handle (`vendor/zed/crates/gpui/src/
    /// elements/uniform_list.rs`'s own `UniformListScrollHandle`, verified against
    /// `vendor/zed/crates/git_ui/src/git_panel.rs`'s real `commit_history_scroll_handle` use of
    /// the same type) - `track_scroll`'d by [`Self::render_file_view`]'s own `uniform_list` and
    /// driven by [`Self::navigate_to_definition`]/[`Self::spawn_file_load`]'s completion handler/
    /// [`Self::render_file_view`]'s own already-fresh-navigation branch, whichever actually lands
    /// a real go-to-definition target line on [`Self::code_cursor`] - never called on every
    /// render (an ordinary click-driven `code_cursor` change, or a plain freshly opened file
    /// starting at line 1, has no reason to fight the user's own current scroll position).
    /// Without this, F12 updated [`Self::code_cursor`] and the status bar's `ln N` text but never
    /// moved the actual viewport, so navigating to a distant line was invisible until the user
    /// scrolled there themselves.
    file_view_scroll_handle: UniformListScrollHandle,
    /// The real, cached parse/highlight of whichever file [`Self::render_file_view`] most
    /// recently loaded - `code_view::load_file`/`code_view::highlight_rust` run at most once per
    /// real file-content change, never once per render (see [`Self::render_file_view`]'s docs for
    /// the exact real staleness check, `code_view::cache_is_fresh`, that decides whether this is
    /// reused or refreshed on any given render), and - since this phase's fix for the blocking-
    /// I/O bug [`FileLoadState`]'s own docs describe - always written from the background-
    /// executor task [`Self::spawn_file_load`] dispatches, never synchronously during `render()`.
    file_view_cache: Option<code_view::ParsedFile>,
    /// The real path and wall-clock time [`Self::render_file_view`] last actually called
    /// `std::fs::metadata` for its freshness check, throttling that stat syscall to at most once
    /// per [`FILE_FRESHNESS_CHECK_INTERVAL`] rather than running it unconditionally on *every*
    /// render. A blocking `stat()` is cheap in isolation, but `Self::render_file_view` runs on
    /// the GPUI foreground thread on every repaint while a file is open - as often as every
    /// ~33ms during a streaming agent session (see [`Self::palette_file_candidates`]'s docs for
    /// the same scenario) - so an unconditional syscall there is a real, avoidable per-frame
    /// cost, the same class of bug `file_view_cache` itself already exists to avoid for the far
    /// more expensive parse it gates. `None` until the first real check. `Self::select_worktree`
    /// doesn't need to reset this: a worktree switch always changes `file_tree_root`, so the
    /// very next check's `absolute_path` can never match whatever path was last recorded here,
    /// and a path mismatch always forces a real, unthrottled re-check (see that method's docs).
    file_view_last_freshness_check: Option<(PathBuf, Instant)>,
    /// See [`FileLoadState`]'s own docs.
    file_load_state: FileLoadState,
    /// The real changed-line set (`code_view::changed_line_set`) for whichever `DiffFile`
    /// [`Self::open_diff_file_cache`] currently holds - recomputed only by
    /// [`Self::refresh_open_diff_file_cache`] (the one real point where its only input,
    /// `open_diff_file_cache`, can change), never per render. `Self::render_file_view` used to
    /// fold this from scratch (up to `wt_core::diff`'s own 2000-hunk-line-per-file cap) on every
    /// single render of the File view, right next to the parse cache this same phase already
    /// takes care to avoid rebuilding every frame.
    file_view_changed_lines: HashSet<usize>,
    /// The File view's real "last click" cursor line (1-indexed - `design_handoff_jerry_ade/
    /// README.md`'s status bar `ln 44, col 14`), set by [`Self::render_file_view_line`]'s own
    /// click handler and by [`Self::spawn_file_load`]'s completion handler whenever a newly
    /// loaded file resets it to `1`. Column tracking is a documented scope simplification: real
    /// per-character hit-testing against a monospace text run wasn't implemented this phase (see
    /// this crate's report for that phase), so - following the exact same standard
    /// `Self::render_file_status_bar`'s own docs already apply to omitting a fabricated
    /// `rust-analyzer` status field - no column is shown at all rather than a fabricated `col 1`
    /// that never actually reflects where the user clicked.
    code_cursor: Option<usize>,
    /// Whether the command palette (⌘K) overlay is open - `design_handoff_jerry_ade/README.md`'s
    /// "Added state: palette_open".
    palette_open: bool,
    /// The palette's real active scope (`All`/`Commands`/`Files`) - `design_handoff_jerry_ade/
    /// README.md`'s "Added state: palette_scope".
    palette_scope: palette::PaletteScope,
    /// The palette's real, currently typed query - deliberately the same minimal hand-rolled
    /// append/backspace text field as `Self::filter_query` (see `Self::handle_filter_key_down`'s
    /// docs for why a small, deliberate subset was chosen over `vendor/zed/crates/gpui/examples/
    /// input.rs`'s full `EntityInputHandler`).
    palette_query: String,
    /// The palette's real, currently highlighted result row - an index into the flattened
    /// (`crate::palette::flatten`) row order of whatever `Self::build_palette_groups` most
    /// recently produced, moved by `↑`/`↓` and run by `⏎` (`Self::handle_palette_key_down`).
    palette_selected: usize,
    palette_focus_handle: FocusHandle,
    /// Whatever real focus target [`Self::open_palette`] found via `window.focused(cx)` right
    /// before it moved focus onto [`Self::palette_focus_handle`] - `None` if nothing was
    /// focused yet (a completely fresh window). [`Self::close_palette`] restores this on close
    /// so ⌘K's own focus target isn't left dangling on a node that stops being rendered the
    /// moment the palette closes (see that method's docs for the bug this fixes: without a
    /// restore, `Window::focus` keeps pointing at the untracked `palette_focus_handle`, and
    /// every subsequent action dispatch - including the very next ⌘K - falls back to the root
    /// node instead of reaching `Self::handle_toggle_palette_action`).
    palette_return_focus: Option<FocusHandle>,
    /// Which session was active when [`Self::open_palette`] most recently ran - compared
    /// against the active session at close time so [`Self::close_palette`] can tell whether
    /// [`Self::palette_return_focus`] is still safe to restore. A palette-spawned "New Shell"
    /// (or any other command that calls [`Self::new_session`]) swaps which session is active,
    /// and the centre pane only ever renders `sessions.active()` (see the module docs) - so a
    /// captured pre-open handle belonging to the *previous* active session's terminal pane
    /// would be exactly as untracked/stale as `palette_focus_handle` itself once that swap
    /// happens. When the active session changed while the palette was open, `close_palette`
    /// ignores the captured handle and focuses the *current* active session's pane instead.
    palette_opened_session: Option<SessionId>,
    /// The palette's real file-candidate list (`crate::palette::FileCandidate`, one per
    /// non-directory [`Self::file_tree`] entry, up to `file_tree::MAX_ENTRIES` = 5000) - built
    /// once by [`Self::rebuild_palette_file_candidates`] at the two real points its inputs
    /// change ([`Self::load_file_tree`]'s and [`Self::load_diff`]'s completion handlers), not
    /// rebuilt by [`Self::build_palette_groups`] itself. `Self::build_palette_groups` used to
    /// clone every entry's `PathBuf` and allocate two new `String`s per entry (via
    /// `changes::split_dir_name`) from scratch on *every* call - and it's called from
    /// `Self::render_palette` (so on every render while the palette is open, which can be as
    /// often as every ~33ms during a streaming agent session - see `Self::tree_change_marks`'s
    /// docs for the same "live session calls `cx.notify()` on every output chunk" scenario),
    /// plus again on every arrow key and every `⏎`. With up to 5000 entries that was the same
    /// "recompute expensive work every render instead of caching" cost `file_view_cache`/
    /// `open_diff_file_cache`/`tree_change_marks` were already fixed for elsewhere in this file.
    /// Sessions/commands candidates are deliberately *not* cached the same way - they're bounded
    /// by the number of open tabs (a handful) plus a fixed 10 commands, and a session's status
    /// dot is genuinely live per-render data (`Self::session_status` reads the pane's current
    /// `is_running()`/`idle_duration()` directly) with no stable mutation point to invalidate a
    /// cache on; caching them would trade a real perf problem for a real staleness bug for no
    /// measurable gain.
    palette_file_candidates: Vec<palette::FileCandidate>,
    /// The session rail's real, user-adjustable width - `design_handoff_jerry_ade/README.md`'s
    /// Layout table ("276, range 240–340"), dragged via the resize handle on the rail's right
    /// edge (see [`Self::apply_pane_resize`]/`crate::layout::rail_width_for_cursor`).
    rail_width: Pixels,
    /// The files/changes panel's real, user-adjustable width - see [`Self::rail_width`]'s docs
    /// for the same mechanism, mirrored on the panel's left edge
    /// (`crate::layout::panel_width_for_cursor`).
    panel_width: Pixels,
    /// The window body's real, current paint bounds - captured every render by a `gpui::canvas`
    /// child of the body div (see [`Self::render`]'s body child list), and read by
    /// [`Self::apply_pane_resize`] to turn a drag's absolute cursor position into a pane width.
    /// Verified against the real, equivalent `Workspace::bounds` field
    /// `vendor/zed/crates/workspace/src/workspace.rs` captures the same way for its own
    /// dock-resize `on_drag_move` handler. `Bounds::default()` (zero origin/size) until the
    /// first paint; harmless, since nothing reads it before a resize handle can be dragged.
    body_bounds: gpui::Bounds<Pixels>,
    /// Armed by a left mouse-down on the title bar's drag area, consumed (and cleared) by
    /// the next mouse-move to call the real `Window::start_window_move` - see
    /// `Self::render_title_bar`'s docs for why this two-step arm-then-move dance is needed
    /// rather than starting the move directly on mouse-down (verified against the same
    /// real pattern `vendor/zed/crates/platform_title_bar/src/platform_title_bar.rs` uses).
    title_bar_move_armed: bool,
    /// The session rail's grouping mode - `design_handoff_jerry_ade/README.md`'s `by urgency
    /// ▾ / by project ▾` control. See `crate::rail::RailMode`.
    rail_mode: RailMode,
    /// The rail's real filter query - typed via `Self::handle_filter_key_down`, actually
    /// filters the rendered session/worktree rows in both grouping modes (see
    /// `crate::rail::filter_sessions`/`filter_project_children`), not a decorative
    /// placeholder.
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
    /// Real, bounded disk-usage total across every listed worktree (see
    /// `crate::rail::disk_usage_bytes`'s docs for the real `std::fs` walk and its cap),
    /// recomputed whenever the worktree list reloads. `None` while the very first
    /// computation is still in flight.
    disk_usage: Option<(u64, bool)>,
    /// The real, per-worktree half of the same computation [`Self::disk_usage`] sums -
    /// `crate::rail::disk_usage_bytes(path)` run once per real, readable worktree path (see
    /// `Self::load_disk_usage`). `disk_usage` itself is always exactly the sum/any-truncated
    /// fold of this map, kept as its own field only because most call sites (the rail footer)
    /// only ever need the aggregate, not a lookup - added for the Settings › Worktrees page
    /// (`Self::render_settings_worktrees_page`), which is the first real caller that needs a
    /// *per-row* size rather than just the total.
    worktree_disk_usage: HashMap<PathBuf, (u64, bool)>,
    /// Feedback from the most recent `prune` click - a real outcome (how many worktrees were
    /// actually removed, or a real error from `wt_core::remove_worktree`), shown in the rail
    /// footer until the next prune attempt or worktree reload.
    prune_status: Option<String>,
    /// `true` after one click on the footer `prune` button, cleared after the second
    /// (confirming) click actually removes anything, or by any other rail interaction in the
    /// meantime (switching rail mode, selecting a session/worktree, or editing the filter -
    /// see each of those handlers) - see `Self::request_prune`'s docs for why prune is a
    /// real two-click confirmation rather than a single unconfirmed destructive click.
    prune_confirm_armed: bool,
    /// Whether the Settings surface (`design_handoff_jerry_ade/README.md`'s "Settings"
    /// section) is currently replacing the three-zone body - see [`Self::open_settings`]/
    /// [`Self::close_settings`], which follow the exact same real-focus-capture-and-restore
    /// shape [`Self::palette_open`]'s own docs describe (and the same bug class this app has
    /// already hit once for the palette - see `palette_focus_tests`).
    settings_open: bool,
    /// Which Settings nav page is currently selected - persists across opens/closes (unlike
    /// the palette's query/scope, which resets every time) so navigating away and reopening
    /// Settings later doesn't lose your place.
    settings_page: settings::SettingsPage,
    settings_focus_handle: FocusHandle,
    /// See [`Self::palette_return_focus`]'s docs for the exact bug this mirrors and fixes -
    /// same mechanism, applied to the Settings surface instead of the palette overlay.
    settings_return_focus: Option<FocusHandle>,
    /// See [`Self::palette_opened_session`]'s docs - same mechanism, applied to Settings.
    settings_opened_session: Option<SessionId>,
    /// The Settings › Agents page's real rows - `crate::settings::detect_agent_rows`, resolved
    /// via `pty_core::resolve_on_path`, but computed off the foreground thread and cached here
    /// (see [`Self::load_agent_rows`]) rather than recomputed inline on every render.
    /// `resolve_on_path`'s not-found path walks every `$PATH` entry with no early exit -
    /// measured on this machine at ~30ms for a genuinely absent binary like `codex`, which is
    /// exactly the case this page exists to show - so calling it directly from `render()` would
    /// have capped the whole Settings surface at that render's frame rate, and re-paid the ~30ms
    /// on every one of `start_status_polling`'s 3s re-renders while the page stayed open.
    /// `Vec::new()` (an empty card, matching a `None` `resolved_path` for a still-loading state
    /// would be dishonest here since it isn't per-row) until the first real load completes.
    agent_rows: Vec<settings::AgentRow>,
    /// The context bar's real `Merge` action and Surface D's real conflict-resolution flow -
    /// see `crate::merge::MergeFlow`'s docs. `None` whenever no session has an in-flight
    /// merge attempt or unresolved conflict - `Self::render_center_pane` shows the active
    /// session's normal pty/diff surface in that case, exactly as before this phase.
    merge_flow: Option<merge::MergeFlow>,
    /// `true` for the duration of a real, in-flight `Complete merge`/`Abort merge` background
    /// git operation (`Self::complete_merge_flow`/`Self::abort_merge_flow`) - guards against a
    /// second click spawning a second, racing real git operation while the first is still
    /// running (see those methods' own docs for the real race this closes: a fast
    /// Abort-right-after-Complete double-click could otherwise let `git merge --abort` win a
    /// race against an in-flight `git commit` and discard real, already-resolved conflict
    /// work). `Self::start_merge` doesn't need a second flag of its own for the same purpose -
    /// its own `self.merge_flow.is_some()` check already serves that role, since `merge_flow`
    /// is `None` right up until the moment it synchronously sets it to `Running`.
    merge_op_in_flight: bool,
    _load_worktrees_task: Option<Task<()>>,
    _load_file_tree_task: Option<Task<()>>,
    _load_diff_task: Option<Task<()>>,
    /// The real, in-flight `code_view::load_file` background task [`Self::spawn_file_load`]
    /// dispatched for whichever path [`FileLoadState::Loading`] currently names - dropping this
    /// (e.g. by assigning a fresh one, or clearing it in `Self::select_worktree`'s per-worktree
    /// reset) cancels that load immediately, per the same real GPUI `Task` semantics
    /// [`Self::_merge_cleanup_task`]'s own docs describe.
    _file_load_task: Option<Task<()>>,
    _status_poll_task: Option<Task<()>>,
    _disk_usage_task: Option<Task<()>>,
    _prune_task: Option<Task<()>>,
    _agent_rows_task: Option<Task<()>>,
    _merge_task: Option<Task<()>>,
    /// A real, in-flight `Self::clear_merge_flow_for_closed_session` best-effort abort, kept in
    /// its own field rather than sharing [`Self::_merge_task`] - see that method's docs for the
    /// exact real bug this was verified to cause when the two shared one slot: dropping a GPUI
    /// `Task` cancels it immediately (`vendor/zed/crates/scheduler/src/executor.rs`'s own "If
    /// you drop a task it will be cancelled immediately"), so a cleanup-triggered abort
    /// overwriting the same field as a real, in-flight `Self::complete_merge_flow`/
    /// `Self::abort_merge_flow` commit would cancel that commit mid-flight, permanently
    /// stranding [`Self::merge_op_in_flight`] at `true` (its own reset lives inside the very
    /// closure that got cancelled) and letting `git merge --abort` win a race against `git
    /// commit`, discarding already-resolved conflict work.
    _merge_cleanup_task: Option<Task<()>>,
    /// Every real, in-flight [`Self::resolve_active_hunk`] background write
    /// (`wt_core::merge::write_resolved_file`), keyed by nothing (a `Vec`, not a single slot) -
    /// see that method's docs for why a single `Option<Task<()>>` here was a verified real bug:
    /// resolving one file's last hunk while a *different* file's write was still in flight
    /// would drop (cancel) the earlier write via the same "dropping a `Task` cancels it
    /// immediately" mechanism described on [`Self::_merge_cleanup_task`], leaving real conflict
    /// markers on disk while the in-memory model already reported that file as resolved. Writes
    /// to distinct files are independent, so keeping every in-flight one alive here is safe;
    /// [`Self::resolve_active_hunk`] prunes already-finished entries (`Task::is_ready`) before
    /// pushing a new one so this never grows unboundedly.
    _merge_write_tasks: Vec<Task<()>>,
    /// A real `lsp_core::LspClient` per repository root (`design_handoff_jerry_ade/README.md`'s
    /// "Language server UI" Diagnostic state) - keyed by [`Self::file_tree_root`] at the time a
    /// Rust file was first opened under it, since a session's worktree (not necessarily
    /// [`Self::repo_path`] itself) is what `Self::render_file_view` actually resolves paths
    /// against. One real `rust-analyzer` process is spawned lazily (see
    /// [`Self::ensure_lsp_client`]) the first time a `.rs` file opens under a given root, and
    /// reused for every subsequent Rust file opened under that same root within this session -
    /// never respawned per file. See [`LspClientState`]'s own docs for the three real states a
    /// client can be in.
    ///
    /// ## Eviction: kill-on-switch, not unbounded, not a bounded LRU
    ///
    /// Left alone, this map would only ever grow: nothing used to remove an entry once its root
    /// stopped being the active worktree, so browsing N different worktrees (each with a Rust
    /// file opened) leaked N live `rust-analyzer` processes for the rest of the window's life -
    /// against this repo's own workspace (path-deps on the entire vendored `zed` tree), multiple
    /// real GB per leaked instance. [`Self::evict_stale_lsp_clients`] (called from
    /// [`Self::select_worktree`] on every switch) now tears down every entry whose key is not
    /// the newly active [`Self::file_tree_root`]. This is a deliberate "kill the non-active one"
    /// choice over a small bounded LRU (e.g. keeping the 2-3 most recently active roots warm to
    /// avoid respawn latency when a user bounces between two worktrees): worktree switches are a
    /// deliberate, relatively infrequent user action (not a per-keystroke hot path), an LRU would
    /// still leak unboundedly-but-slower across a long session browsing many worktrees, and this
    /// repo's own real per-instance memory cost makes "keep more than one warm" an expensive
    /// default to carry for a latency win that only matters for the specific two-worktrees-
    /// bounced-between case. See [`Self::evict_stale_lsp_clients`]'s own docs for how a real
    /// `Arc<LspClient>` clone that might still be alive at eviction time (an in-flight
    /// `dispatch_did_open`/poll-loop reference) is handled.
    lsp_clients: HashMap<PathBuf, LspClientState>,
    /// Real, absolute paths that have already had a real `textDocument/didOpen` sent for their
    /// owning [`Self::lsp_clients`] entry - checked by [`Self::render_file_view`] so reopening
    /// (or re-rendering) an already-open file never re-sends `didOpen` with a stale `version`.
    /// Never removed on file close: this is a deliberate, documented choice not to send a
    /// matching `textDocument/didClose` for a read-only viewer - see [`Self::dispatch_did_open`]'s
    /// docs for the reasoning.
    lsp_opened_files: HashSet<PathBuf>,
    /// The current File view's real, per-line diagnostic index (`crate::diagnostics_view::
    /// index_diagnostics_by_line`) for whichever Rust file [`Self::render_file_view`] most
    /// recently rendered - recomputed at the start of every `render_file_view` call for a Rust
    /// file (a cheap `HashMap` clone-and-fold over however many real diagnostics rust-analyzer
    /// has published, not a re-parse), and read by the row builder closure `uniform_list`
    /// constructs. Cleared (not just stale) whenever a non-Rust file or no file is showing, so a
    /// diagnostic from a previously open file can never bleed into a different file's rows.
    file_view_diagnostics: HashMap<usize, Vec<diagnostics_view::LineDiagnostic>>,
    /// Every real, in-flight `lsp_core::LspClient::spawn`/`did_open` background task - a `Vec`
    /// for the same reason [`Self::_merge_write_tasks`] is one (independent real operations
    /// against potentially-different repo roots/files; dropping an unrelated in-flight one would
    /// cancel it via the same real GPUI `Task`-drop-cancels semantics documented there), pruned
    /// of already-finished entries (`Task::is_ready`) before each push.
    _lsp_tasks: Vec<Task<()>>,
    /// The single, long-lived background poll loop that watches every ready
    /// [`Self::lsp_clients`] entry's `lsp_core::LspClient::drain_updates()` wake channel and calls
    /// `cx.notify()` when a real `publishDiagnostics` notification has arrived - started lazily
    /// (see [`Self::ensure_lsp_poll_task`]) the first time any client becomes `Ready`, and kept
    /// alive for the rest of the window's life (unlike the other `Option<Task<()>>` fields here,
    /// this one is deliberately never reset to `None` after being set - there is always at most
    /// one real poll loop needed for however many clients exist).
    _lsp_poll_task: Option<Task<()>>,
    /// Surface C's real Hover-state cache (`design_handoff_jerry_ade/README.md`'s "Language
    /// server UI" Hover state) - the outcome of the most recent real click-triggered
    /// `textDocument/hover` request (see [`Self::request_hover`]), or `None` before the first
    /// click/after switching files. Caching discipline mirrors [`Self::file_view_cache`]'s own:
    /// [`Self::request_hover`] is the *only* place this is written, and it's a real no-op (no new
    /// request dispatched) when called again for the exact same `(path, line, byte_range)` this
    /// already holds - never recomputed/re-requested on every render the way an earlier version
    /// of this crate's diagnostics indexing once was (see `Self::render_file_view`'s own docs on
    /// that fixed bug). Also doubles as [`Self::trigger_goto_definition`]'s real target: there is
    /// no separately-tracked "symbol under consideration" in this read-only viewer - see
    /// [`GotoDefinition`]'s own docs for why that's an honest choice, not a shortcut.
    hover: Option<HoverEntry>,
    /// The single real, in-flight [`Self::request_hover`] background task, if any - a single
    /// `Option` slot (not an unbounded `Vec`, unlike [`Self::_lsp_tasks`]/
    /// [`Self::_goto_definition_tasks`]'s own "independent operations" reasoning) *because* hover
    /// requests are never independent of each other: [`Self::hover`] only ever shows one entry at
    /// a time, so a real click while a previous request is still in flight always supersedes it -
    /// the previous request's eventual result would just be discarded by
    /// [`Self::request_hover`]'s own `still_current` check regardless. Assigning a fresh task here
    /// (rather than pushing onto a `Vec`) drops the previous one immediately, which is what closes
    /// the real bug this replaced: rapid clicking during `rust-analyzer`'s initial indexing (a
    /// real `textDocument/hover` request can block a background-executor thread for the full real
    /// `LSP_QUERY_TIMEOUT`, 10s) used to let an unbounded number of real, concurrently in-flight
    /// requests accumulate, pinning that many pool threads and starving other real background work
    /// (file loads, git status refresh) that share the same executor.
    _hover_request_task: Option<Task<()>>,
    /// Every real, in-flight [`Self::trigger_goto_definition`] background task - a `Vec`, unlike
    /// [`Self::_hover_request_task`]'s single slot, for the same real "independent operations,
    /// dropping an unrelated one would cancel it" reasoning [`Self::_lsp_tasks`]'s own docs give:
    /// a real `F12` press has no analogous `still_current` short-circuit tying it to a single
    /// live UI slot the way hover's own single `Self::hover` field does. Pruned of already-
    /// finished entries (`Task::is_ready`) before each push.
    _goto_definition_tasks: Vec<Task<()>>,
    /// A real, one-shot "the next completed load of *this exact file* should land the cursor on
    /// this line, not line 1" instruction for [`Self::spawn_file_load`]'s completion handler - set
    /// by [`Self::navigate_to_definition`] when a real go-to-definition result named a file that
    /// wasn't already the open one (so a real background load/parse has to happen first; see that
    /// method's own docs for the race this exists to avoid: without it, `spawn_file_load`'s
    /// completion handler unconditionally resetting [`Self::code_cursor`] to `1` for *every* file
    /// load - the right default for an ordinary file-tree/Changes-row click - would silently
    /// overwrite a real navigation target the instant the newly-opened file's background parse
    /// finished).
    ///
    /// Keyed by the real target path (not just a bare line number) so a *different* file's
    /// completed load can never misapply it - a real, deterministically reproducible bug this
    /// fixes: `F12` to a not-yet-cached file B sets this to `(B, 5)`; without the path check, a
    /// second navigation click into an unrelated file C *before* B's own load resolves would let
    /// C's own completion handler apply B's stale `5` (B's own in-flight load task got silently
    /// cancelled - dropping a `Task` cancels it immediately, see [`Self::_file_load_task`]'s own
    /// docs - the moment C's `spawn_file_load` replaced it, so B's completion handler never ran to
    /// consume this and clear it). Consumed (`Option::take`, only when the path matches) the first
    /// time it's read, in either [`Self::render_file_view`] itself (when the target file's cache
    /// was already fresh, so `spawn_file_load` never even runs) or `spawn_file_load`'s own
    /// completion handler (when a real load was dispatched) - whichever actually applies it first.
    /// Explicitly cleared (regardless of path) by [`Self::open_file_view`] on every fresh open -
    /// so a stale entry from an abandoned navigation can never leak onto a *third*, unrelated
    /// file's later load - and by `spawn_file_load`'s own failed-load branch, so a real read error
    /// can't leave it to misapply onto whatever loads successfully next.
    pending_cursor_line: Option<(PathBuf, usize)>,
    /// The real, config-file-backed settings struct (`design_handoff_jerry_ade/revision/
    /// CHANGELOG.md`'s 2026-07-29 entry, change 3) - loaded once from `~/.config/jerry/
    /// settings.toml` at startup (`Self::new`, via `Settings::load_or_init`) and re-saved
    /// (`Self::persist_settings`) on every real change made from the General/Appearance &
    /// scaling/Themes settings pages or the command palette's `Window controls: …` entries.
    /// See `crate::settings_store`'s own module docs for exactly which fields are
    /// persisted-only vs. also applied, and [`Self::window_controls_style`] for the one field
    /// this struct exposes through a dedicated accessor rather than a bare field read.
    settings: Settings,
    /// The real, resolved path [`Self::persist_settings`] writes to - `Some(~/.config/jerry/
    /// settings.toml)` in production (`Self::new`), `None` for every GPUI test built via
    /// `root::focus::palette_focus_tests::open_test_app` (`Self::new_with_settings`'s own
    /// docs) - see [`Self::persist_settings`] for why a `None` here makes a save a genuine,
    /// honest no-op rather than a special-cased test skip.
    settings_path: Option<PathBuf>,
    /// The single, real, in-flight [`Self::persist_settings`] background save, if any - unlike
    /// [`Self::_lsp_tasks`]'s own "independent operations" `Vec`, settings saves are never
    /// independent of each other: there is only ever one real `settings.toml` to write, so a
    /// fast second settings change must *supersede* an earlier still-mid-write save, not race
    /// it. This used to be a `Vec`, with each save capturing its own `Settings` snapshot at
    /// *spawn* time - a real, live bug: two fast edits (e.g. clicking a stepper twice quickly)
    /// had no ordering guarantee between their two independent `std::fs::write` calls, so the
    /// file could end up holding the *older* edit's value if its write happened to land last,
    /// silently reverting the newer one on disk (and, on next launch, in memory) even though
    /// the UI kept showing the newer value. Assigning a fresh task to this single slot instead
    /// drops (and so immediately cancels, per GPUI's real "dropping a `Task` cancels it
    /// immediately" semantics - the same real mechanism [`Self::_hover_request_task`]'s own docs
    /// rely on for this exact "supersede the previous pending op" shape) any still-in-flight
    /// previous save. And unlike the old per-task snapshot, [`Self::persist_settings`]'s spawned
    /// closure reads [`Self::settings`] fresh, at the moment it actually runs, not at the moment
    /// it was spawned - so even in the residual window where a superseded save's background
    /// `std::fs::write` was already dispatched to a thread-pool worker before cancellation could
    /// stop it (the same real, disclosed, already-accepted race
    /// [`Self::_hover_request_task`]'s own docs describe for a superseded hover request), it was
    /// still writing *some* real, live snapshot of the settings at the time it ran, never a
    /// stale one captured before a later edit happened.
    _settings_save_task: Option<Task<()>>,
    /// The config banner's real `TOML | JSON` segment (`design_handoff_jerry_ade/revision/
    /// CHANGELOG.md`'s change 3) - a display-only choice, not itself a [`Settings`] field, see
    /// `crate::settings_store`'s "TOML is the real file; JSON is a read-only alternate view"
    /// docs.
    settings_cfg_format: CfgFormat,
    /// The Settings › Language servers page's real rows - `crate::settings::detect_lsp_rows`,
    /// resolved via `pty_core::resolve_on_path`, cached the same way [`Self::agent_rows`] is
    /// (see that field's own docs for the real ~30ms-per-not-found-binary cost this avoids
    /// paying on every render).
    lsp_rows: Vec<settings::LspRow>,
    _lsp_rows_task: Option<Task<()>>,
    /// The Keybindings settings page's real, hand-rolled filter query - same minimal
    /// append/backspace shape as [`Self::filter_query`] (see
    /// [`Self::handle_settings_keymap_filter_key_down`]).
    settings_keymap_filter: String,
    settings_keymap_filter_focus_handle: FocusHandle,
}

impl AdeApp {
    /// The real, single source of truth for which platform's title-bar variant/keycap glyphs
    /// render right now (`design_handoff_jerry_ade/CHANGELOG.md`'s 2026-07-29 entry, changes 1
    /// and 2 - see `crate::keymap`'s module docs for why this one field drives both).
    /// [`Self::settings`]`.window.controls` is the real, persisted backing (R3) - both the
    /// General settings page's `Window controls` choice row and the command palette's three
    /// `Window controls: …` entries read/write it through this same accessor and
    /// [`Self::set_window_controls_style`], never a second, independent copy.
    pub(super) fn window_controls_style(&self) -> WindowControlsStyle {
        self.settings.window.controls
    }

    /// Sets [`Self::window_controls_style`] and persists it for real (`Self::persist_settings`).
    /// The one real write path both the General settings page and the command palette's three
    /// `Window controls: …` entries call, so the two can never silently disagree about which
    /// override is active.
    pub(super) fn set_window_controls_style(
        &mut self,
        style: WindowControlsStyle,
        cx: &mut Context<Self>,
    ) {
        self.settings.window.controls = style;
        self.persist_settings(cx);
        cx.notify();
    }

    /// Spawns a real, background-executor save of the *current* [`Self::settings`] value to
    /// [`Self::settings_path`] (`Settings::save_at`) - called from every real settings mutation
    /// (see `Self::set_window_controls_style` and the Appearance/Themes page mutators in
    /// `crate::root::settings_render`). A `None` path (every GPUI test - see
    /// [`Self::settings_path`]'s own docs) makes this a genuine no-op, not a special test case;
    /// a real save failure against a real, resolved path is logged, not surfaced to the UI.
    ///
    /// Deliberately re-reads [`Self::settings`] via `this.update` *inside* the spawned task,
    /// rather than cloning it into the closure up front - see [`Self::_settings_save_task`]'s own
    /// docs for the real out-of-order-write bug this fixes, and why a single superseding task
    /// slot alone isn't quite enough on its own to close it. Mirrors the real "gather on
    /// foreground, do blocking work on background" shape `Self::start_status_polling`/
    /// `Self::load_worktrees` already use, just with the "gather" step being a single field
    /// clone rather than a real `git`/`gix` call.
    pub(super) fn persist_settings(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let Ok(settings) = this.update(cx, |this, _cx| this.settings.clone()) else {
                // The entity is already gone (e.g. the window closed while this task was still
                // queued) - nothing real left to save.
                return;
            };
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
        });
        // A single slot, not an unbounded `Vec` - see `Self::_settings_save_task`'s own docs.
        // Assigning here drops (and so immediately cancels) any still-in-flight previous save.
        self._settings_save_task = Some(task);
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
            .when(self.palette_open, |el| el.child(self.render_palette(cx)))
    }
}

impl AdeApp {
    /// The three-zone workspace body (session rail, centre pane, files/changes panel) -
    /// pulled out of [`Render::render`] so it and [`Self::render_settings`] can both be
    /// [`gpui::AnyElement`]-branched as a single child, matching the real precedent found for
    /// this exact shape (`vendor/zed/crates/gpui/src/util.rs`'s `Styled::when_else`, and
    /// `vendor/zed/crates/gpui/examples/image_loading.rs`'s own `.into_any_element()` fallback
    /// branch) rather than trying to conditionally omit/include zones within one shared tree.
    fn render_workspace_body(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("body")
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            // Real drag-to-resize: `on_drag_move` fires for every mouse-move event
            // while a `PaneResizeDrag` is active *anywhere in the window*, not just
            // while the cursor stays over the thin resize handle that started the drag
            // (see `Interactivity::on_drag_move`'s own doc comment, and
            // `PaneResizeDrag`'s docs for the real, verified
            // `vendor/zed/crates/workspace` precedent this follows) - registering it
            // here, on the body that contains both handles, is what makes a fast drag
            // still track correctly even after the cursor leaves the handle's own
            // 6px-wide hitbox. No matching `on_mouse_up` is needed to end the drag:
            // GPUI's own window dispatch clears `active_drag` (and stops delivering
            // further `on_drag_move` ticks) on *any* `MouseUpEvent`, anywhere in the
            // window, independent of which element's handlers actually fired -
            // verified at `vendor/zed/crates/gpui/src/window.rs`'s
            // `dispatch_mouse_event` ("If this was a mouse up event, cancel the active
            // drag"). Combined with [`Self::apply_pane_resize`] deriving the width
            // fresh from the cursor position and [`Self::body_bounds`] on every tick
            // rather than an armed baseline, there is no drag state left here to leak
            // if the mouse is released outside the window.
            .on_drag_move(cx.listener(
                |this, event: &DragMoveEvent<PaneResizeDrag>, _window, cx| {
                    let PaneResizeDrag(target) = *event.drag(cx);
                    this.apply_pane_resize(target, event.event.position.x, cx);
                },
            ))
            // Captures the body's real paint bounds into `Self::body_bounds` on every
            // render, the same real `gpui::canvas` pattern
            // `vendor/zed/crates/workspace/src/workspace.rs` uses to capture its own
            // `Workspace::bounds` for the equivalent dock-resize computation - see
            // `Self::body_bounds`'s docs.
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

/// The shared real focus-save-on-open/restore-on-close tail for every overlay/surface that
/// captures a pre-open focus target and must restore it on close: [`AdeApp::close_palette`],
/// [`AdeApp::close_settings`], and [`AdeApp::close_change_diff`] all had this exact same block
/// (verified, not just skimmed, to be identical in logic - not merely superficially similar -
/// before extracting it: same `session_changed` comparison, same "skip the captured handle in
/// favor of the active session's pane" fallback order, same unconditional clear of both fields
/// at the end), parameterized only by which pair of fields (a surface's own
/// `*_return_focus`/`*_opened_session`) each caller passes in - `AdeApp::palette_return_focus`'s
/// own docs describe the real dangling-focus bug this whole pattern exists to fix, found and
/// fixed three separate times (Phases E, F, H3) against three hand-copied blocks before this
/// extraction.
///
/// If the active session changed while the surface was open (`*opened_session` no longer matches
/// the real current active session), any captured pre-open handle is skipped in favor of the
/// *current* active session's terminal pane - a captured handle from a session that's no longer
/// active would be exactly as untracked/stale as the surface's own focus handle once that swap
/// happens. Otherwise the captured handle is restored, falling back to the active session's
/// terminal pane if nothing was focused before (e.g. a completely fresh window that had never
/// been clicked into). A free function (not an `AdeApp` method) because every caller already
/// holds `&mut self` and needs to pass `&mut self.some_field` alongside it - a method taking
/// `&mut self` couldn't also borrow one of `self`'s own fields as a separate `&mut` parameter.
///
/// Deliberately does not call `cx.notify()` - every real caller has its own additional
/// surface-specific state to change (`palette_open`/`settings_open`/`open_change`, etc.) around
/// this call and already issues its own single `cx.notify()` once all of it (this restore
/// included) is done.
fn restore_focus(
    sessions: &Sessions,
    return_focus: &mut Option<FocusHandle>,
    opened_session: &mut Option<SessionId>,
    window: &mut Window,
    cx: &mut App,
) {
    let session_changed = sessions.active_id() != *opened_session;
    let restore_target = if session_changed {
        None
    } else {
        return_focus.take()
    };
    let focus_target = restore_target.or_else(|| {
        sessions
            .active()
            .map(|session| session.pane.focus_handle(cx))
    });
    if let Some(handle) = focus_target {
        window.focus(&handle, cx);
    }
    *return_focus = None;
    *opened_session = None;
}

mod code_surface;
mod focus;
mod lsp;
mod merge_flow;
mod palette_render;
mod rail_render;
mod resize;
mod settings_render;
mod settings_widgets;
mod sidebar_render;
mod state;
mod status_bar;
mod title_bar;
mod widgets;
mod work_surface_render;
