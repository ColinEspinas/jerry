//! The top-level three-pane window: a left worktree sidebar, a tabbed center pane of
//! terminal agents, and a right file tree, composed as GPUI entities.
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
//! ## One rail row per worktree; agents are tabs scoped to it
//!
//! [`crate::work_surface::agents::Agents`] holds any number of independent, simultaneously-running
//! terminal agents (a plain shell, or an agent CLI), each pinned to the worktree it was
//! started in. The agent rail shows exactly one row per worktree
//! (`crate::rail::state::WorktreeRow`, aggregating every agent open in it), and the centre pane's tab
//! strip (`AdeApp::render_tab_strip`) only ever shows the *currently selected* worktree's own
//! agents - never a flat, unscoped list of every agent across every worktree.
//!
//! Selecting a worktree in the sidebar still never spawns or kills anything - but, unlike
//! before this revision, it *does* change which agent is "active"
//! (`crate::work_surface::agents::Agents::activate_for_worktree`, called from [`AdeApp::select_worktree`]):
//! the active agent must always belong to the selected worktree, or the centre pane would show
//! one worktree's terminal while the rail highlights another. [`AdeApp::selected`] itself still
//! drives the file tree, and which worktree `active_agent_cwd` resolves to for the *next* "New
//! terminal"/"New agent pane" click - that part is unchanged. Spawning an agent is still always
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
use crate::code_surface::markdown_preview;
use crate::env_info;
use crate::graph_view;
use crate::keymap::WindowControlsStyle;
use crate::keymap_overrides;
use crate::lsp::diagnostics as diagnostics_view;
use crate::merge::state as merge;
use crate::palette::state as palette;
use crate::rail::repo::{self as repo, Repo, RepoId};
use crate::rail::state as rail;
use crate::rail::worktrees::{self, WorktreeItem};
use crate::settings::custom_theme;
use crate::settings::state as settings;
#[cfg(test)]
use crate::settings::state::SettingsPage;
use crate::settings::store::{self as settings_store, CfgFormat, Settings};
use crate::sidebar::changes::{self, ChangeTag};
use crate::sidebar::file_tree;
use crate::sidebar::fold_state;
use crate::sidebar::tree_ops;
use crate::status_bar::process_stats;
use crate::text_history;
use crate::theme;
use crate::title_bar::menu as title_bar;
use crate::updater;
use crate::work_surface::agents::{AgentId, AgentKind, Agents};
use crate::work_surface::state as work_surface;
use crate::worktree_history::flow as worktree_history;

use crate::code_surface::state::{
    BlameCacheEntry, BlameLoadState, CommitMessageState, DiffLoadState, FileLoadState, HoverAnchor,
    HoverEntry,
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
// `JumpToAgent1`..`JumpToAgent8` are eight distinct zero-sized actions, one per keystroke,
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
        NewAgent,
        TogglePalette,
        ToggleSettings,
        GotoDefinition,
        NewTerminal,
        NewAgentPane,
        NewGitGraph,
        NextChangedFile,
        JumpToAgent1,
        JumpToAgent2,
        JumpToAgent3,
        JumpToAgent4,
        JumpToAgent5,
        JumpToAgent6,
        JumpToAgent7,
        JumpToAgent8,
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
        TextUndo,
        TextRedo,
        CloseFocusedTab,
        FileTreeRename,
        FileTreeCopy,
        FileTreeCut,
        FileTreePaste,
        FileTreeDelete,
        FileTreeUndo,
        FileTreeRedo,
        TerminalClear,
        TerminalCopy,
        TerminalPaste,
    ]
);

/// How often `crate::rail::state::compute_status_snapshot`'s background `git` status/diff refresh
/// re-runs. Coarser than `crate::terminal::pane`'s 8ms poll since this spawns real `git` child
/// processes per worktree/agent path, not a cheap channel `try_recv`.
pub(crate) const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// GitHub issue #12's "keep a low-frequency polling fallback" - the worktree list is re-parsed
/// at least this often even if [`AdeApp::_worktree_watcher`] never fires at all (watcher setup
/// failed, or the specific change has no filesystem-watchable signature at all - deleting a
/// worktree's own directory by hand touches nothing under `$GIT_COMMON_DIR`). See
/// `crate::rail::render::AdeApp::start_worktree_watch`'s docs for how this combines with the
/// watcher's own near-instant path.
pub(crate) const WORKTREE_WATCH_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How often [`AdeApp::start_worktree_watch`]'s loop checks
/// [`crate::rail::worktree_watch::DirtyFlag`] between [`WORKTREE_WATCH_POLL_INTERVAL`] ticks -
/// short enough that a real watcher event reaches [`AdeApp::load_worktrees`] within "roughly a
/// second" (the issue's own latency target) rather than waiting for the next 5s poll.
pub(crate) const WORKTREE_WATCH_TICK: Duration = Duration::from_millis(300);

/// After the dirty flag is first observed set, how long [`AdeApp::start_worktree_watch`] waits
/// before actually refreshing - the issue's "a single `git worktree add` touches several files;
/// collapse to one refresh" debounce: a burst of events within this settle window collapses into
/// the one refresh that follows it, rather than each one triggering its own.
pub(crate) const WORKTREE_WATCH_SETTLE: Duration = Duration::from_millis(200);

/// GitHub issue #13's own "keep a low-frequency polling fallback" - the file tree is re-walked
/// at least this often even if [`AdeApp::_file_tree_watcher`] never fires (setup failed, or an
/// OS-level watch-descriptor budget was exhausted on a very large tree - see
/// `crate::sidebar::file_tree_watch::spawn_file_tree_watcher`'s own docs). Same interval as
/// [`WORKTREE_WATCH_POLL_INTERVAL`], not independently tuned - there's no reason the two should
/// disagree about what an acceptable "worst case, no watcher at all" staleness looks like.
pub(crate) const FILE_TREE_WATCH_POLL_INTERVAL: Duration = WORKTREE_WATCH_POLL_INTERVAL;

/// [`AdeApp::start_file_tree_watch`]'s own [`WORKTREE_WATCH_TICK`]/[`WORKTREE_WATCH_SETTLE`].
pub(crate) const FILE_TREE_WATCH_TICK: Duration = WORKTREE_WATCH_TICK;
pub(crate) const FILE_TREE_WATCH_SETTLE: Duration = WORKTREE_WATCH_SETTLE;

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
    /// Every git repository the user has added (Revision R12 Phase 0 - see [`repo`]'s
    /// module docs). Zero-to-many: the app's *current* single-repo-per-window behaviour is just
    /// the common case of this list holding exactly one entry, not a separate code path - see
    /// [`Self::add_repo`]. Order is insertion order and carries no meaning of its own (a later
    /// rail-rendering phase orders *groups* by urgency, not by this `Vec`'s order -
    /// `design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.0).
    pub(crate) repos: Vec<Repo>,
    /// Which of [`Self::repos`] is "the" repo for every currently-single-repo-scoped piece of
    /// state this app still has (the file tree, the diff, a fresh agent's cwd, `worktrees`
    /// below) - see [`Self::focused_repo`]/[`Self::focused_repo_path`]. `None` only when
    /// [`Self::repos`] is empty, which nothing in this phase's startup path (`Self::
    /// new_with_settings`) ever produces - it always adds and focuses exactly one repo, mirroring
    /// today's single-CLI-argument launch.
    pub(crate) focused_repo: Option<RepoId>,
    /// The next [`RepoId`] [`Self::add_repo`] will assign - a plain, process-local monotonic
    /// counter (see [`RepoId`]'s own docs for why it's never derived from a path or persisted).
    /// Seeded past every id already assigned while restoring [`Self::repos`] from
    /// [`repo::RepoState`] at startup, so a freshly-added repo can never collide with one
    /// just loaded from disk.
    pub(crate) next_repo_id: u64,
    /// The resolved path [`Self::persist_repo_state`] writes to - a sibling of
    /// [`Self::settings_path`]/[`Self::fold_state_path`], `None` for the same tests that get a
    /// `None` settings path (see [`repo::repo_state_path_for`]).
    pub(crate) repo_state_path: Option<PathBuf>,
    /// The [`repo::repo_key`]s this instance has added a repo under (and, once a later phase
    /// adds a way to remove one, would mark removed too - [`repo::RepoState::save_merged_at`]'s
    /// own contract already covers both) - what [`Self::persist_repo_state`] hands
    /// `RepoState::save_merged_at` as "mine to overwrite". See [`Self::fold_state_owned`]'s
    /// identical role for the equivalent whole-file-clobber this prevents when a second `jerry`
    /// process (open against a different repo) is writing the same `repos.toml` at the same
    /// time.
    pub(crate) repo_state_owned: std::collections::BTreeSet<String>,
    /// The repo-list file's own serial writer loop task - see [`Self::_fold_state_save_task`]'s
    /// identical role/reasoning, just for `repos.toml` instead of `file-tree-state.toml`.
    pub(crate) _repo_state_save_task: Option<Task<()>>,
    /// See [`Self::fold_state_save_pending`] - same contract, for the repo-list file.
    pub(crate) repo_state_save_pending: bool,
    /// See [`Self::fold_state_save_running`] - same contract, for the repo-list file.
    pub(crate) repo_state_save_running: bool,
    /// Every worktree of [`Self::focused_repo`], as read by `wt_core::list_worktrees` -
    /// deliberately still a flat, single-repo list rather than living on the [`Repo`] itself
    /// (see [`Repo::worktrees`]'s own docs): this phase is data-model-and-persistence only, and
    /// rewiring every consumer of "the worktree list" onto a per-repo one is the rail-rendering
    /// phase's job, not this one's.
    pub(crate) worktrees: Vec<WorktreeItem>,
    pub(crate) worktrees_error: Option<String>,
    /// GitHub issue #12's "the user is notified" half of selection recovery - set by
    /// [`AdeApp::load_worktrees`] (via `crate::rail::worktrees::recover_selection`) when a
    /// refresh finds the previously selected worktree gone (or newly
    /// [`crate::rail::worktrees::WorktreeItem::is_broken`]) and falls [`Self::selected`] back to
    /// the main worktree. Rendered as a dismissible banner
    /// (`crate::rail::render::AdeApp::render_worktree_selection_notice_banner`), mirroring
    /// [`Self::tree_op_error`]'s own "small, visible, honest error surface" convention. Cleared
    /// either by that click-to-dismiss, or implicitly the next time the user makes a real
    /// selection ([`AdeApp::select_worktree`]/[`AdeApp::select_worktree_by_path`]) - never by a
    /// later refresh on its own, since refreshes happen every few seconds and a notice that
    /// vanished before it could be read would defeat the point of showing it.
    pub(crate) worktree_selection_notice: Option<String>,
    /// The agent rail's own real overlay scrollbar handle (GitHub issue #30) - a plain
    /// `gpui::ScrollHandle`: `crate::rail::render::AdeApp::render_rail_list` renders every row
    /// eagerly, not through a `uniform_list`.
    pub(crate) rail_scroll_handle: gpui::ScrollHandle,
    pub(crate) selected: Option<usize>,
    pub(crate) agents: Agents,
    pub(crate) file_tree: file_tree::FileTree,
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
    /// Which agent(s) wrote each file in [`Self::diff_state`]'s currently loaded diff
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §4's `by: 's1'`/`by:
    /// ['s1','s9']` record) - see [`changes::Authorship`]'s own docs for why this is always empty
    /// today (no real authorship tracking exists yet; this phase only defines the shape a later
    /// one populates). Reset alongside [`Self::diff_state`] on every reload
    /// (`crate::code_surface::tabs::AdeApp::load_diff`), never carried across one.
    pub(crate) file_authorship: changes::Authorship,
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
    /// Whether the last completed file-tree walk was a *complete* inventory of the worktree's
    /// directories (`file_tree::FileTreeListing::is_complete`) - false when it silently skipped
    /// an unreadable or too-deep subdirectory. The only condition under which
    /// `AdeApp::prune_stale_fold_state` may read "not in this listing" as "deleted".
    ///
    /// There is no `file_tree_truncated` companion any more: GitHub issue #160 removed the walk's
    /// entry cap, so "the walk stopped early because it ran out of budget" is no longer a state
    /// this app can be in.
    pub(crate) file_tree_complete: bool,
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
    /// GitHub issue #148: which directory row a real in-progress file-tree drag is currently
    /// hovering, if any - the drop-target highlight `crate::sidebar::render::AdeApp::
    /// render_file_tree_row` paints, and the folder `Self::move_paths_into_dir` moves into on a
    /// real drop. `None` whenever no drag is over the tree at all.
    pub(crate) tree_drag_hover_target: Option<PathBuf>,
    /// Real, already-applied file-tree operations (delete, rename, cut/paste move) that
    /// `crate::sidebar::tree_ops::AdeApp::undo_tree_op` can reverse, most recent last - the
    /// file-tree's own undo history, distinct from each text field's own `text_history::
    /// TextField` undo and from `wt_core::undo`'s git-level commit/discard undo. GitHub issue
    /// #105: delete no longer asks for confirmation - it happens immediately, and this (plus
    /// [`Self::tree_undo_backup_root`] for a delete's real content) is what makes that safe.
    pub(crate) tree_undo_stack: Vec<tree_ops::TreeUndoEntry>,
    /// Entries [`Self::undo_tree_op`] has popped, most recently undone last - `crate::sidebar::
    /// tree_ops::AdeApp::redo_tree_op`'s own source. Cleared by the next real (non-undo/redo)
    /// tree operation, same as every other undo/redo stack in this app.
    pub(crate) tree_redo_stack: Vec<tree_ops::TreeUndoEntry>,
    /// Monotonic counter naming each delete's backup file/dir under [`Self::tree_undo_backup_root`],
    /// guaranteeing two deletes of same-named files never collide there, without pulling in a
    /// UUID dependency for something this local.
    pub(crate) tree_undo_backup_counter: u64,
    /// This `AdeApp` instance's own share of [`Self::tree_undo_backup_root`], assigned once at
    /// construction from a process-wide atomic counter. `std::process::id()` alone identifies the
    /// *process*, not the instance - every `#[gpui::test]` in a `cargo test` binary runs in the
    /// same process, so two tests each starting `tree_undo_backup_counter` back at 0 would
    /// otherwise write their first delete's backup to the exact same path, and the second test's
    /// backup would silently fail with `AlreadyExists` (or worse, its cleanup would delete the
    /// first test's still-referenced backup). This is what actually caused GitHub issue #105's
    /// undo/redo delete test to fail intermittently in full-suite runs while always passing alone.
    pub(crate) tree_undo_instance_id: u64,
    /// The most recent file-operation failure (a refused rename, a failed trash command),
    /// surfaced under the tree rather than dropped into the log - the same small, honest error
    /// surface [`Self::file_save_error`] uses for a failed save.
    pub(crate) tree_op_error: Option<String>,
    /// The file tree's keyboard-focus target. `track_focus`'d by
    /// `crate::sidebar::render::AdeApp::render_file_tree`'s container, which is also the node
    /// carrying the `"file-tree"` `key_context` every tree keybinding is scoped to - so
    /// `Ctrl+C`/`Ctrl+X`/`Ctrl+V` can never match while a terminal agent has focus. See
    /// `crate::sidebar::tree_ops`'s module docs.
    pub(crate) tree_focus_handle: FocusHandle,
    /// The file tree container's real painted bounds, captured by a `gpui::canvas` child each
    /// render (the same pattern [`Self::plus_button_bounds`] uses) - where a *keyboard*-opened
    /// context menu (`Shift+F10`) anchors, since there is no cursor position to use.
    pub(crate) file_tree_bounds: gpui::Bounds<Pixels>,
    /// Every in-flight confirmed delete (a real `gio trash` child process, or a real
    /// `remove_dir_all`) - a real `Vec`, not one slot, since GitHub issue #145's bulk delete
    /// starts one real task per selected path in the same call: a single `Option` overwritten in
    /// a loop would drop (and, being a `Task`, therefore cancel) every delete but the last one.
    /// Finished tasks are never removed - `Task<()>` completing is a no-op to poll again, and
    /// this only ever grows by a handful of entries per bulk delete, not worth the bookkeeping to
    /// prune.
    pub(crate) _tree_delete_tasks: Vec<Task<()>>,
    /// The in-flight Duplicate / paste-a-copy - a real, recursive `std::fs` tree copy, run on the
    /// background executor rather than in the click listener that started it (see
    /// `crate::sidebar::tree_ops::AdeApp::spawn_tree_copy`). One slot, superseding: a second copy
    /// started while one is running drops the first *task handle*, which cannot stop a copy
    /// already in progress - deliberately, since abandoning one half-way is strictly worse than
    /// letting it finish, and the two have different destinations (each is
    /// `file_ops::unique_destination`-resolved) so they cannot collide with each other.
    pub(crate) _tree_copy_task: Option<Task<()>>,
    /// Per-file staging state for the Changes list (Revision R12 §5: "the checkbox **is**
    /// staging, not 'reviewed'") - a file's path is in this set iff its checkbox is checked, i.e.
    /// it would be included in the commit composer's next commit **and is really staged in the
    /// real git index** (`crate::sidebar::render::AdeApp::toggle_staged` does a real, immediate
    /// `git add`/unstage - see its own docs). `Self::render_changes_header`'s progress bar/count
    /// and `Self::render_commit_composer` both read this directly. Keyed by worktree, not agent
    /// (§5's own "staging is keyed by worktree, not agent") - it holds exactly one set at a time,
    /// cleared synchronously on every worktree switch by
    /// `crate::root::state::reset_per_worktree_ui_state` (so nothing from the worktree just left
    /// can flash before the new one's real state lands) and then genuinely re-derived from the
    /// real git index (`wt_core::stage::staged_paths`) inside `Self::load_diff`'s own background
    /// task - see that method's docs for why this is a live re-query on every diff load rather
    /// than a per-worktree cache, and why that means a worktree with something already staged in
    /// real git before Jerry ever opened it still reads as staged once loaded.
    pub(crate) staged_files: HashSet<PathBuf>,
    /// The most recent real staging/unstaging failure from [`Self::toggle_staged`] - `(path,
    /// message)`. Surfaced next to the commit composer (`Self::render_staging_error`) the same
    /// honest way [`Self::tree_op_error`] surfaces a failed tree operation, rather than silently
    /// swallowing a real `git add`/`git reset` failure behind an optimistic UI update that
    /// quietly reverts. Cleared on dismiss, on the next successful toggle of the same path, and
    /// (implicitly, since it names a worktree-relative path) whenever a worktree switch clears
    /// [`Self::staged_files`] out from under it.
    pub(crate) staging_error: Option<(PathBuf, String)>,
    /// Every in-flight [`Self::toggle_staged`] background `git add`/`git reset` - a [`TaskPool`],
    /// not a single slot, for the same "independent operations" reason as
    /// [`Self::_merge_write_tasks`]: two different Changes rows' checkboxes clicked in quick
    /// succession are two genuinely independent real git operations, and a shared single slot
    /// would silently cancel (and so leave un-applied) whichever one didn't win the race for the
    /// slot.
    pub(crate) _stage_tasks: TaskPool,
    /// Whether the commit composer's `▾` split-button popover (Revision R12 §5: *Commit and
    /// push* / *Commit all N files* / *Amend last commit* / *Stash staged files*) is open. Closed
    /// on every worktree switch (`Self::select_worktree`) since it targets that worktree's own
    /// staged set, whenever any other [`menus::MenuSurface`] opens, when the right sidebar leaves
    /// the Changes view (the composer stops being rendered, and a latched-open flag would pop the
    /// popover back up on return), and when the window loses focus.
    pub(crate) commit_menu_open: bool,
    /// The commit composer's own painted bounds, captured every render by a `gpui::canvas` child
    /// (the same pattern [`Self::plus_button_bounds`] uses). `Bounds::default()` until the
    /// composer has really painted once.
    ///
    /// GitHub issue #176: the popover used to be a *child* of the composer, which made its
    /// "click-away dismisses" scrim - `absolute()` with inset 0 - resolve against the composer's
    /// own ~135px box instead of the window, so clicking the Changes list, the file tree, the
    /// rail, the tab strip or the editor did not close it ("the commit one is particularly bugged
    /// and hard to close"). It is now a sibling of `Self::render_plus_menu` in [`Render::render`]
    /// with a genuinely full-window scrim, which means it has to carry its anchor in window space
    /// like every other popover here does.
    pub(crate) commit_composer_bounds: gpui::Bounds<Pixels>,
    /// Ordered list of currently-open file tabs, rendered after every agent's own tab by
    /// `Self::render_tab_strip` - **per worktree**, keyed by [`Self::file_tree_root`]
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §3: "Switching worktrees
    /// swaps the whole strip. Each worktree remembers its own ... open files"). No duplicates
    /// within one worktree's list: opening an already-open file just activates its existing entry
    /// (`Self::push_open_file`). Removed only on explicit tab close (`Self::close_file_tab`) -
    /// **not** on a worktree switch: these are worktree-*relative* paths, which is exactly why a
    /// flat, unscoped list would be collision-prone (two worktrees sharing a relative path) if it
    /// weren't split per worktree like this. Read through [`Self::open_files`] (a method, not this
    /// field, outside this struct) and written through [`Self::open_files_mut`] - both resolve the
    /// *current* worktree's entry, creating it empty on first access rather than requiring a
    /// separate "new worktree" seeding step.
    pub(crate) open_files_by_worktree: HashMap<PathBuf, Vec<PathBuf>>,
    /// Which file tab (if any) the centre pane is showing instead of an agent -
    /// `Some(path)` iff `path` is also in [`Self::open_files`]. Set by a Changes row
    /// (`Self::open_change_diff`), a Files-tree row (`Self::open_file_view`), or an already-open
    /// tab (`Self::activate_file_tab`); cleared by selecting an agent tab or closing the active
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
    /// Changes row's own selection highlight. GitHub issue #145: also the multi-selection's
    /// *anchor* - the row a plain click lands on, a Shift+click range starts from, and the only
    /// row F2/rename ever targets. See [`Self::additional_tree_selection`]'s own docs for the
    /// rest of a real multi-selection.
    pub(crate) selected_tree_path: Option<PathBuf>,
    /// GitHub issue #145: every multi-selected file-tree row *besides* the anchor
    /// ([`Self::selected_tree_path`]) - Ctrl/Cmd+click toggles membership, Shift+click replaces
    /// this with the range between the anchor and the clicked row. The tree's real selection is
    /// always this set plus the anchor together (`Self::tree_selected_paths`), never either alone,
    /// and it's a `HashSet` rather than a `Vec` since membership (`Self::is_tree_path_selected`,
    /// read on every visible row every frame) is the only real query this needs - row order comes
    /// from the tree itself, not from this field, whenever an operation needs an ordered list.
    pub(crate) additional_tree_selection: HashSet<PathBuf>,
    /// Surface C's `File | Diff` toggle for whichever file [`Self::open_change`] names - set to
    /// `Diff` by [`Self::open_change_diff`] and `File` by [`Self::open_file_view`], read by
    /// [`Self::render_code_surface`] alongside a "does this file even have a diff" check (a
    /// diff-less file always renders as `File` regardless of this field).
    pub(crate) code_view: code_view::CodeView,
    /// GitHub issue #115: a `.md` file's `Source | Preview` toggle - reset the same way
    /// [`Self::code_view`] is (see that field's own docs), not persisted per tab.
    pub(crate) markdown_view: markdown_preview::MarkdownView,
    /// Scroll position for [`Self::render_markdown_preview`] - plain [`gpui::ScrollHandle`]
    /// rather than [`Self::file_view_scroll_handle`]'s `UniformListScrollHandle`, since the
    /// preview is a real nested block tree, not a virtualized flat line list.
    pub(crate) markdown_preview_scroll_handle: gpui::ScrollHandle,
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
    /// Whether the git graph tab (GitHub issue #1, phase (a)) exists in the tab strip. Unlike
    /// [`Self::open_files`] there is at most one - "one per window" per the design spec - so this
    /// is a plain `bool`, not a collection.
    pub(crate) graph_tab_open: bool,
    /// Whether the git graph tab is the tab strip's currently *active* entry -
    /// `crate::work_surface::render::AdeApp::render_center_pane` shows it whenever this is
    /// `true`, taking priority over [`Self::open_change`]/an agent pane. Switching to an agent
    /// or file tab sets this back to `false` without closing the graph tab, exactly mirroring how
    /// [`Self::open_change`] behaves for file tabs; closing the graph tab outright
    /// (`crate::graph_view::render::AdeApp::close_git_graph_tab`) always sets it `false` too.
    pub(crate) graph_tab_active: bool,
    /// The git graph tab's own keyboard-focus target, `track_focus`'d by
    /// `crate::graph_view::render::AdeApp::render_graph_view`'s container.
    pub(crate) graph_focus_handle: FocusHandle,
    /// Whether [`Self::graph_focus_handle`] is genuinely focused right now (GitHub issue #127) -
    /// kept as a plain bool, set directly alongside each real `window.focus(&self.
    /// graph_focus_handle, ...)` call (`crate::graph_view::render::AdeApp::open_git_graph`/
    /// `Self::toggle_graph_row_menu`) and cleared in `crate::graph_view::render::AdeApp::
    /// leave_graph_tab`, rather than read live at render time or driven by a `cx.on_focus`
    /// subscription. Two real reasons, not one: the row list's own render call chain
    /// (`crate::graph_view::render::AdeApp::render_graph_row`, reached through `Self::
    /// render_center_pane`) never carries a real `&Window` to check `FocusHandle::is_focused`
    /// against (`render_center_pane` is also called as a bare "force a redraw" helper from dozens
    /// of non-rendering call sites with no window at all); and a `cx.on_focus`/`cx.on_blur`
    /// subscription registered at construction - the shape `Self::wire_caret_blink` uses
    /// successfully for other handles - was live-tested here and never fired, since
    /// `graph_focus_handle` is only ever `track_focus`'d conditionally (only while
    /// `Self::graph_tab_active`) and the very first focus of it can happen before that node has
    /// ever been part of a rendered frame.
    pub(crate) graph_view_focused: bool,
    /// Pre-open focus target for [`Self::graph_focus_handle`] - see [`OverlayFocus`]. Captured
    /// only on the closed-to-open transition and moved on only when something else becomes the
    /// active centre-pane content - see `crate::graph_view::render::AdeApp::leave_graph_tab`.
    pub(crate) graph_focus: OverlayFocus,
    /// The git graph tab's own state (loaded rows, scope, selection, right panel) - see
    /// `crate::graph_view::state::GraphTabState`.
    pub(crate) graph_state: graph_view::state::GraphTabState,
    /// The in-flight `wt_core::graph::build_graph` background load, one slot - a fresh load
    /// supersedes an older one still running, mirroring [`Self::_load_diff_task`].
    pub(crate) _load_graph_task: Option<Task<()>>,
    /// The in-flight `wt_core::graph::commit_changed_files` background load behind the Commit
    /// panel's "Files changed" list - one slot, same shape as [`Self::_load_graph_task`].
    /// `commit_changed_files` performs real blocking I/O (spawns `git show`), so it must never
    /// run inline in a render method - see `crate::graph_view::render::AdeApp::
    /// load_commit_files`'s own docs for the real bug this replaced.
    pub(crate) _load_commit_files_task: Option<Task<()>>,
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
    /// Real, live per-tab text-editing state for the File view (Revision R8.5a) - keyed by
    /// **both** the owning worktree ([`Self::file_tree_root`] at the time the buffer was created)
    /// and the worktree-relative path, exactly like [`Self::open_files_by_worktree`]'s own outer
    /// key, so an unsaved edit survives a worktree switch away and back
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §1/§3) and two worktrees that
    /// happen to share a relative path can never merge or overwrite each other's in-memory
    /// content. Created lazily the first time a file is opened in File view (see
    /// [`crate::code_surface::file_view::AdeApp::render_file_view`]), seeded from the exact same
    /// background read [`Self::spawn_file_load`] already performs. [`Self::file_view_cache`]
    /// stays the freshness-check/diagnostics/hover source of truth (the last-*saved* snapshot,
    /// per this phase's own scope); this map is what's actually on screen and what an explicit
    /// save writes. Deliberately **not** removed on an ordinary tab close
    /// ([`crate::code_surface::tabs::AdeApp::close_file_tab`]) **or** a worktree switch -
    /// dropping unsaved edits just because a tab was closed or a different worktree was clicked
    /// (with no "save before closing?"/"discard this edit?" prompt - the design's own explicit
    /// call: fluidly hopping between worktrees must never carry that friction) would be a real,
    /// silent data-loss risk; reopening the same file (in the same worktree) later restores the
    /// exact in-memory buffer. Read through [`Self::edit_buffer`]/written through
    /// [`Self::edit_buffer_mut`]/[`Self::insert_edit_buffer`]/[`Self::remove_edit_buffer`] for
    /// every *synchronous* call site (these resolve the worktree half of the key from whatever
    /// [`Self::file_tree_root`] is right now); a real `cx.spawn` task that reads or writes a
    /// buffer *after* an `.await` must instead go through [`Self::edit_buffer_at`]/
    /// [`Self::edit_buffer_at_mut`] with a `cwd` captured **before** the await - by the time such
    /// a task resumes, [`Self::file_tree_root`] may already name a different worktree entirely
    /// (the user switched away while the task was in flight), and resolving the key from the
    /// *current* one at that point would silently read or write the wrong worktree's buffer -
    /// exactly the stale-async-task bug class `Self::load_file_tree`'s own `file_tree_root`
    /// identity guard exists to prevent, applied here to a keyed map instead of a single field.
    /// (Editor zoom used to be reset on a worktree switch too - see `settings_store`'s "Editor
    /// zoom is one global, persisted number now" docs for why it no longer is: it moved to
    /// `Settings.appearance.editor_zoom_percent`, a real persisted field, not per-worktree UI
    /// state.)
    pub(crate) edit_buffers: HashMap<(PathBuf, PathBuf), edit_buffer::EditBuffer>,
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
    /// Pre-open focus target and active agent for [`Self::palette_focus_handle`] - see
    /// [`OverlayFocus`]/[`restore_focus`].
    pub(crate) palette_focus: OverlayFocus,
    /// The palette's file-candidate list (`crate::palette::state::FileCandidate`, one per
    /// non-directory [`Self::file_tree`] entry) - built once when `file_tree`/the diff reload
    /// (on the background executor for the tree walk, see [`Self::load_file_tree`]; via
    /// [`Self::rebuild_palette_file_candidates`] for the diff), not rebuilt on
    /// every `Self::build_palette_groups` call (which runs on every render while the palette is
    /// open, up to ~30x/sec during a streaming agent). Agent/command candidates aren't
    /// cached the same way: they're few, and an agent's status dot is genuinely live per-render
    /// data with no stable invalidation point.
    pub(crate) palette_file_candidates: Vec<palette::FileCandidate>,
    /// The agent rail's user-adjustable width (240-340px), dragged via the resize handle on
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
    /// The rail's filter query - filters the rendered worktree/agent rows (see
    /// `crate::rail::state::filter_worktree_rows`). Carries a real per-widget undo history
    /// (GitHub issue #17 - see [`text_history::TextField`]); unlike the palette's, this widget
    /// lives for the whole agent, so its history does too.
    pub(crate) filter_query: text_history::TextField,
    /// Explicit per-worktree expand/collapse overrides for the rail's worktree rows
    /// (`design_handoff_jerry_ade/revision 3/REVISION-2026-07-31.md` §2.2: "caret state is
    /// remembered per worktree"), keyed by worktree path. Absence means "use the default" -
    /// [`crate::rail::render::AdeApp::worktree_is_expanded`] is the one place that default is
    /// decided (collapsed for a worktree whose most urgent agent is idle, expanded otherwise) -
    /// so a worktree the user has never touched the caret on tracks that live default rather
    /// than freezing whatever it happened to be the first time it rendered.
    pub(crate) rail_collapse_overrides: HashMap<PathBuf, bool>,
    pub(crate) filter_focus_handle: FocusHandle,
    /// The rail's *root container*'s focus handle - the app's real "nowhere else to put focus"
    /// fallback target (`Self::select_worktree`, `Self::close_agent`, `Self::cancel_new_file`),
    /// deliberately **not** [`Self::filter_focus_handle`].
    ///
    /// Those three sites used to fall back onto the filter field itself, which an adversarial
    /// audit found had become a real, reachable bug once GitHub issue #17 tagged that field
    /// `"text-input"`: closing the last agent focused a text input the user never asked to type
    /// in, and `Ctrl+Z` there resolved to `TextUndo` against an empty field - a silently swallowed
    /// keystroke with no feedback. The rail's root div carries no key context of its own, so
    /// focusing *it* keeps the focused `FocusId` genuinely findable in the next rendered frame -
    /// the actual invariant the fallback exists to protect - without claiming to be a text widget.
    pub(crate) rail_focus_handle: FocusHandle,
    /// GitHub issue #90's empty-state view's own focus target (`Self::render_empty_state`) - the
    /// same "focus something real rather than leave `Window::focus == None`" discipline
    /// [`Self::rail_focus_handle`]'s own docs give, applied to a window with no repo focused at
    /// all, where the rail (and everything else `render_workspace_body` renders) isn't part of
    /// the tree in the first place.
    pub(crate) empty_state_focus_handle: FocusHandle,
    /// Real `+N -M`/has-changes totals per worktree or agent cwd, refreshed by the
    /// periodic background task started in `Self::new` - see `crate::rail::state::
    /// compute_status_snapshot`'s docs. Read (never written outside that task's completion
    /// callback) by `Self::build_agent_rows` each render.
    pub(crate) diff_cache: HashMap<PathBuf, rail::DiffSummary>,
    /// Real clean/merged notes per worktree path, from the same periodic refresh as
    /// [`Self::diff_cache`] - powers "by project" mode's agent-less worktree rows and the
    /// rail footer's `prune` action.
    pub(crate) worktree_notes: HashMap<PathBuf, rail::WorktreeNote>,
    /// Real `wt_core::diff::AheadBehind` counts per worktree/agent cwd, from the same
    /// periodic refresh as [`Self::diff_cache`] - the status bar's `↑2 ↓0` indicator for the
    /// active agent's worktree.
    pub(crate) ahead_behind_cache: HashMap<PathBuf, wt_core::diff::AheadBehind>,
    /// Real, live per-pid CPU%/memory samples for every currently open agent's process
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
    /// `Some(kind)` for the duration of any in-flight "keep all changes"/"discard worktree"
    /// operation, naming *which* one - not just a bare `bool` - so `Self::render_pty_footer`'s
    /// busy label ("keeping…"/"discarding…") can honestly reflect what's actually running instead
    /// of guessing from which button happens to be visible (a real, live-reproduced bug an audit
    /// caught: a running "keep all changes" made every visible `Discard worktree` button across
    /// every agent read "discarding…"). A single field shared across both, not two independent
    /// guards: these are the only operations that ever mutate real git history or a worktree's
    /// own existence for this feature, so fully serializing them (a second click of either while
    /// one is in flight is a no-op, mirroring [`Self::prune_in_flight`]'s own
    /// single-flag-per-feature precedent) is sufficient, on its own, to make "a slow op racing a
    /// newer one" structurally impossible - there can never be a second one in flight to race
    /// with.
    pub(crate) worktree_history_op_in_flight: Option<worktree_history::WorktreeHistoryOpKind>,
    /// Feedback from the most recent "keep all changes"/"discard worktree" operation, shown in
    /// the status bar
    /// (`status_bar::render::AdeApp::render_status_worktree_history_notice`) until the next one -
    /// deliberately its own render slot, independent of [`Self::prune_status`] (see that
    /// method's own docs for why sharing one slot with `prune_status` was a real bug: an
    /// unrelated prune click could permanently hide every future worktree-history status for the
    /// rest of the agent).
    pub(crate) worktree_history_status: Option<String>,
    /// Real GitHub-releases-backed update detection (GitHub issue #87, `crate::updater`) - the
    /// single source of truth for the status bar's update chip
    /// (`crate::updater::render::AdeApp::render_status_update_notice`) and every real
    /// `self_update` background call's own effect on it (`crate::updater::flow`). `Idle` covers
    /// "never checked", "genuinely up to date", and "the last *check* failed" alike - see
    /// [`updater::state::UpdateState`]'s own docs for why those three collapse into one variant.
    pub(crate) update_state: updater::state::UpdateState,
    /// Guards [`Self::check_for_update`] against a second, racing check spawning while one is
    /// already in flight (the periodic loop's own tick landing on top of a manual palette
    /// click, or vice versa) - the same single-flag-per-feature discipline
    /// [`Self::prune_in_flight`]/[`Self::worktree_history_op_in_flight`] already establish.
    pub(crate) update_check_in_flight: bool,
    /// `Some(id)` after one click on agent `id`'s "Discard worktree" footer button, cleared by
    /// most other gestures in the meantime (mirroring [`Self::prune_confirm_armed`]'s own "most
    /// other gestures disarm it" discipline, applied everywhere that field is - see
    /// `crate::worktree_history::flow::AdeApp::request_discard_worktree`'s own docs for why this
    /// destructive-feeling action gets the same two-click confirmation as prune, even though it's
    /// now genuinely undoable). Not a universal "any gesture at all clears it" guarantee, though:
    /// arming *this* field's own sibling ([`Self::prune_confirm_armed`]'s first, arming click)
    /// does not clear this one, and vice versa - only each field's own confirm/cancel/execute
    /// paths, and a handful of other real navigation gestures, clear it.
    pub(crate) discard_confirm_armed: Option<AgentId>,
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
    /// [`crate::merge::state::MergeFlow`]'s docs. `None` when no agent has an in-flight merge or
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
    /// merge-flow-ending point (abort/complete/dismiss/agent-close) and by a fresh
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
    /// The live `notify` filesystem watcher on `$GIT_COMMON_DIR/worktrees` and
    /// `$GIT_COMMON_DIR/HEAD` (GitHub issue #12, `crate::rail::worktree_watch`). Must be kept
    /// alive for as long as the app runs one - dropping it silently stops every OS-level
    /// notification, leaving only [`Self::_worktree_watch_task`]'s 5s poll fallback. `None` in
    /// the (rare, honestly-reported) case `wt_core::git_common_dir`/`notify::recommended_watcher`
    /// themselves failed, e.g. a repo-less `repo_path` - the poll fallback still runs either way.
    pub(crate) _worktree_watcher: Option<notify::RecommendedWatcher>,
    /// The debounced-watcher-plus-poll-fallback refresh loop
    /// (`crate::rail::render::AdeApp::start_worktree_watch`) that calls
    /// [`Self::load_worktrees`] - see that method's own docs.
    pub(crate) _worktree_watch_task: Option<Task<()>>,
    pub(crate) _load_file_tree_task: Option<Task<()>>,
    /// The live `notify` filesystem watcher on [`Self::file_tree_root`] (GitHub issue #13,
    /// `crate::sidebar::file_tree_watch`) - re-armed on a fresh root every real
    /// [`Self::set_file_tree_root`] call, unlike [`Self::_worktree_watcher`]'s "once, for the
    /// whole app lifetime" own scope. Same "`None` on honest setup failure, poll fallback still
    /// runs" contract as that field.
    pub(crate) _file_tree_watcher: Option<notify::RecommendedWatcher>,
    /// The debounced-watcher-plus-poll-fallback refresh loop (`Self::start_file_tree_watch`) that
    /// calls [`Self::load_file_tree`] - see that method's own docs.
    pub(crate) _file_tree_watch_task: Option<Task<()>>,
    pub(crate) _load_diff_task: Option<Task<()>>,
    /// The in-flight `code_view::load_file` task for whichever path [`FileLoadState::Loading`]
    /// names - dropping it (a fresh assignment, or `Self::select_worktree`'s reset) cancels that
    /// load immediately, per GPUI's `Task`-drop-cancels semantics.
    pub(crate) _file_load_task: Option<Task<()>>,
    pub(crate) _status_poll_task: Option<Task<()>>,
    pub(crate) _disk_usage_task: Option<Task<()>>,
    pub(crate) _prune_task: Option<Task<()>>,
    /// The single in-flight "keep all changes"/"discard worktree" background task, guarded by
    /// [`Self::worktree_history_op_in_flight`] - see that field's own docs for why one slot
    /// shared across both is sufficient discipline here.
    pub(crate) _worktree_history_task: Option<Task<()>>,
    /// The real startup-plus-periodic update-check loop
    /// (`crate::updater::flow::AdeApp::start_update_check_loop`) - matches
    /// [`Self::_worktree_watch_task`]'s own "must be kept alive for the real background work to
    /// keep running" discipline. Deliberately *not* reassigned by an individual
    /// `crate::updater::flow::AdeApp::check_for_update` call (including the ones this same loop
    /// itself makes on every tick) - see that method's own docs for the real self-cancellation
    /// hazard reserving this field to the loop alone avoids.
    pub(crate) _update_check_task: Option<Task<()>>,
    /// The single in-flight `crate::updater::flow::AdeApp::start_update_download` background
    /// task - real download+extract+self-replace, guarded the same
    /// single-flight-per-feature way [`Self::_worktree_history_task`] is (see that field's own
    /// docs), keyed off [`Self::update_state`] itself rather than a separate bool, since
    /// `Downloading` already is that "one is in flight" signal.
    pub(crate) _update_download_task: Option<Task<()>>,
    pub(crate) _agent_rows_task: Option<Task<()>>,
    pub(crate) _merge_task: Option<Task<()>>,
    /// `Self::clear_merge_flow_for_closed_agent`'s best-effort abort - kept separate from
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
    /// lsp_diagnostics_confirmed_version`] to answer a stronger question than "was the content
    /// sent": "has the server actually *answered* for it yet".
    pub(crate) lsp_synced_version: HashMap<PathBuf, i32>,
    /// The highest real document version [`Self::schedule_lsp_sync`]'s diagnostics-pull sequence
    /// (or, for a server with no real pull support, the send itself) has *confirmed* an actual
    /// answer for - keyed the same worktree-relative way as [`Self::lsp_last_synced_content`]
    /// (Revision R8.5b audit findings 5/6). While this trails [`Self::lsp_synced_version`], the
    /// server genuinely has the latest edit but hasn't answered for it yet. Written with `.max(..)`, never a
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
    /// The single, real in-flight `completionItem/resolve` request task, if any - mirrors
    /// [`Self::_completions_request_task`]'s own single-slot reasoning: only the item the user is
    /// *currently* looking at in the detail pane is ever worth resolving, so a fresh selection
    /// always supersedes an in-flight resolve for a previous one.
    pub(crate) _completions_resolve_task: Option<Task<()>>,
    /// Which `(path, completions_generation, item index)` triple [`Self::_completions_resolve_task`]
    /// is currently out asking about, if any - cleared when that request's response lands.
    ///
    /// Exists because "superseded" and "answered" are genuinely different states, and conflating
    /// them cost real data: superseding a resolve *cancels* it (dropping a `Task` cancels it), so
    /// an item the user arrowed past never gets an answer. Recording it in
    /// [`Self::completions_resolved`] at dispatch time therefore marked an item answered that
    /// never was, and coming back to it produced no second request - its row and detail pane
    /// stayed pinned to the unresolved item's own fields for as long as the popup lived. Only a
    /// request that is genuinely still on its way is skipped here; a cancelled one is retried.
    pub(crate) completions_resolve_in_flight: Option<(PathBuf, u64, usize)>,
    /// Which `(path, completions_generation, item index into `CompletionsStatus::Ready::items`)`
    /// triples this app has already *had a real answer* for from `completionItem/resolve` -
    /// keyed by [`Self::completions_generation`] (not cleared explicitly) so a stale entry from a
    /// since-replaced server response is simply never looked up again rather than needing its own
    /// cleanup pass. Exists purely to avoid re-requesting the same already-resolved (or
    /// already-failed) item on every render/selection revisit; the growth this leaves behind is
    /// bounded by how many distinct items a user has actually looked at, not by anything
    /// unbounded.
    pub(crate) completions_resolved: std::collections::HashSet<(PathBuf, u64, usize)>,
    /// Every `completionItem/resolve` response that has landed for the *current*
    /// [`Self::completions_generation`], keyed by its index into
    /// `crate::lsp::completion_popup::CompletionsStatus::Ready::items` and already merged over the
    /// item that response describes. Read by the detail pane and by accept; **never** by a row.
    ///
    /// It lives beside the server's response rather than being merged into it, and that is the
    /// whole point. Merging into `items` was what made a completion row visibly fill in - and, for
    /// a `typescript-language-server` auto-import whose inline `detail` is a bare module specifier
    /// and whose resolved `detail` is a signature, visibly *change* - the moment the user arrowed
    /// onto it. Live-reported: "it should not be like this, all data should be here without
    /// needing to select the suggestion." A row now reads only the untouched response, so it is
    /// complete when the popup opens and frozen from then on, by construction rather than by
    /// convention; the detail pane is the one thing a resolve is allowed to fill in.
    ///
    /// Cleared wherever [`Self::completions_generation`] stops describing the same response - the
    /// same points that clear [`Self::completions_resolved`].
    pub(crate) completions_resolved_items:
        std::collections::HashMap<usize, lsp_core::lsp_types::CompletionItem>,
    /// Surface C's real Completions popup state (Revision R8.5b) - `None` when no popup is
    /// showing. Keyed implicitly to whichever [`Self::edit_buffers`] path
    /// [`CompletionsEntry::path`] names; a stale entry for a file that's no
    /// longer open simply never matches [`Self::active_editable_path`] and is treated as absent
    /// by every render/keybinding site that reads it.
    pub(crate) completions: Option<CompletionsEntry>,
    /// The Completions popup's own `uniform_list` scroll handle (GitHub issue #185) - the real
    /// mechanism that replaced the popup's old `MAX_RENDERED_COMPLETION_ITEMS` (12) hard render
    /// cap. `crate::lsp::completion_popup::AdeApp::render_completions_popover`'s `uniform_list` is
    /// `track_scroll`'d with it, `Self::move_completions_selection` drives
    /// `gpui::UniformListScrollHandle::scroll_to_item` off it so keyboard nav keeps the selected
    /// row in view, and `crate::root::scrollbar::AdeApp::render_vertical_scrollbar` reads its
    /// overlay-scrollbar geometry straight off the same handle - not a second, parallel tracking
    /// mechanism, exactly like [`Self::file_tree_scroll_handle`].
    pub(crate) completions_scroll_handle: UniformListScrollHandle,
    /// GitHub issue #30's real overlay scrollbar for the Completions popup's own detail pane -
    /// `crate::lsp::completion_popup::AdeApp::render_completion_detail_pane`'s scrollable
    /// signature+doc region reads its geometry straight off this handle, mirroring
    /// [`Self::hover_card_scroll_handle`]'s identical role for the Hover card's own scrollable
    /// region. Follow-up to the same fix: a genuinely multi-line signature (a pretty-printed
    /// TypeScript utility/generic type) in the detail pane could overflow past the popup's own
    /// height budget and hide the module-path footer beneath it, the same bug the Hover card had.
    pub(crate) completions_detail_scroll_handle: gpui::ScrollHandle,
    /// A real generation counter bumped every time a completions request is dispatched or the
    /// popup is dismissed (`Self::dismiss_completions`) - see [`Self::schedule_lsp_sync`]'s own
    /// docs for the real, live race this closes: an in-flight `textDocument/completion` request
    /// whose *task* wasn't cancelled (e.g. the user pressed Escape, which doesn't touch
    /// [`Self::_completions_request_task`]) must not resurrect a popup the user already dismissed
    /// once its slow response finally arrives. A request's completion handler only ever applies
    /// its result if the generation it captured at dispatch time still matches this field.
    pub(crate) completions_generation: u64,
    /// A real, one-shot flag: `true` right after [`crate::lsp::completion_popup::AdeApp::
    /// accept_active_completion`] splices an accepted item's text into the buffer, consumed
    /// (reset to `false`) by the very next [`Self::prepare_lsp_sync`] call for that edit. The
    /// text an accept splices in routinely still ends in a real identifier character (accepting
    /// a bare `println` leaves the caret right after a real `n`), which - left unchecked - made
    /// `prepare_lsp_sync`'s own completion-trigger check treat the *accept's own edit* as a fresh,
    /// completion-worthy keystroke and immediately reopen the popup, now filtered down to
    /// essentially just the item the user had just picked. Real editors don't do this: accepting
    /// is itself a real signal the user is done with that particular completion, not an invitation
    /// to show it right back. Does not touch the same edit's real `textDocument/didChange` sync at
    /// all - only the completion-request half of that one debounce tick is skipped.
    pub(crate) completions_suppress_next_trigger: bool,
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
    /// Surface C's hover-state cache - the outcome of the most recent pointer-triggered
    /// `textDocument/hover` request (see [`Self::request_hover`]), `None` before the pointer has
    /// rested on a real symbol, after it has moved away again (see [`Self::dismiss_hover`]), or
    /// after switching files.
    pub(crate) hover: Option<HoverEntry>,
    /// GitHub issue #186: the real token the pointer is resting on *right now*, waiting out
    /// [`HOVER_TRIGGER_DELAY`] before a real `textDocument/hover` request is sent for it. Distinct
    /// from [`Self::hover`] on purpose - the card only paints for a real request that has actually
    /// been made, so merely sweeping the pointer across a line of code paints nothing and sends
    /// nothing. Cleared by [`Self::dismiss_hover`] alongside [`Self::hover`].
    pub(crate) hover_pending: Option<HoverAnchor>,
    /// The single in-flight [`HOVER_TRIGGER_DELAY`] timer for [`Self::hover_pending`] - a single
    /// slot for the same reason [`Self::_hover_request_task`] is one: assigning a fresh task drops
    /// (cancels) the previous one, so a pointer sweeping across ten tokens leaves exactly one
    /// armed timer, not ten.
    pub(crate) _hover_debounce_task: Option<Task<()>>,
    /// The single in-flight [`HOVER_HIDE_DELAY`] timer that debounces *clearing* an already-
    /// visible [`Self::hover`] - the hide-side mirror of [`Self::_hover_debounce_task`]'s show-
    /// side delay. Without this, every real token boundary (or plain whitespace gap) the pointer
    /// crosses while sweeping toward some other target synchronously cleared an already-resolved,
    /// visible card, producing a real, reported flash on every sweep rather than only on a
    /// deliberate re-hover. Assigning a fresh task drops the previous one, matching every other
    /// single-slot task field here.
    pub(crate) _hover_hide_task: Option<Task<()>>,
    /// The real painted bounds of the Hover card (GitHub issue #186), captured every frame by its
    /// own `gpui::canvas` - the same one-frame-lag idiom [`Self::body_bounds`] already uses.
    /// [`Self::track_hover_pointer`] reads it to answer "is the pointer on the card itself right
    /// now", which is what keeps the card alive while the user moves onto it to press its own
    /// `F12 definition` footer instead of dismissing it out from under them.
    pub(crate) hover_card_bounds: Option<gpui::Bounds<Pixels>>,
    /// The real painted bounds of the Diagnostic card, mirroring [`Self::hover_card_bounds`]'s
    /// own idiom exactly (captured every frame by its own `gpui::canvas`).
    /// [`Self::track_hover_pointer`] reads it the same way: the Diagnostic card floats over the
    /// code area just like the Hover card, and can just as easily cover a real, different
    /// hoverable token underneath it - a real, reported bug let moving the pointer onto the
    /// card's own painted area trigger that covered token's own hover, which (per
    /// `Self::render_diagnostic_card`'s own hover-vs-diagnostic priority rule) hid the diagnostic
    /// card the user was actually looking at, right out from under them.
    pub(crate) diagnostic_card_bounds: Option<gpui::Bounds<Pixels>>,
    /// GitHub issue #30's real overlay scrollbar for the Hover card's own scrollable header+doc
    /// region (`crate::code_surface::lsp_ui::AdeApp::render_hover_card_content`) reads its
    /// geometry straight off this handle - the same `gpui::ScrollHandle` pattern every other
    /// plain (non-virtualized) scrollable region in this app already uses. Follow-up to the fix
    /// for a genuinely multi-line signature (a pretty-printed TypeScript utility/generic type)
    /// overflowing the card's own fixed height and hiding the footer underneath it - before that
    /// fix landed, nothing in the Hover card could ever overflow, which is why it had none.
    pub(crate) hover_card_scroll_handle: gpui::ScrollHandle,
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
    /// GitHub issue #141: the Themes page's "Generate from colour" seed - a real, focusable hex
    /// input (`#rrggbb`), same minimal append/backspace/`Esc`-clears shape as
    /// [`Self::settings_keymap_filter`] and the same real per-widget undo history (GitHub issue
    /// #17). Its value is what `crate::theme::shift_from_seed` derives a whole theme from.
    pub(crate) theme_seed_input: text_history::TextField,
    pub(crate) theme_seed_focus_handle: FocusHandle,
    /// The real background-executor task behind "Generate from colour" - same one-at-a-time
    /// shape as [`Self::_custom_theme_import_task`].
    pub(crate) _theme_generate_task: Option<Task<()>>,
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
    /// The live `Window::observe_window_activation` subscription that closes every open
    /// dropdown/context menu when the window itself loses OS focus (GitHub issue #176: "when a
    /// dropdown loses focus it should be closed"). Held for the entity's whole lifetime, like
    /// [`Self::_window_appearance_subscription`] beside it - a dropped `Subscription` stops
    /// firing.
    ///
    /// A menu is a transient, click-away surface anchored to a control in *this* window; leaving
    /// one painted while the user is working in another window means coming back to a stale
    /// popover positioned off bounds that may since have moved. Only the six
    /// [`menus::MenuSurface`]s are swept - the palette/Settings/"New file" overlays own real
    /// keyboard focus and half-typed input, and closing those out from under an alt-tab would
    /// destroy real work.
    pub(crate) _window_activation_subscription: Subscription,
    /// Whether the tab strip's `+` menu popover is open - see [`Self::render_plus_menu`].
    /// Closed by its own scrim click, by picking a row, by opening any other
    /// [`menus::MenuSurface`], by the window losing focus, and defensively by
    /// [`Self::open_palette`]/[`Self::open_settings`] (it's rendered as an unconditional sibling
    /// of both, so it would otherwise paint over a surface it no longer makes sense above).
    pub(crate) plus_menu_open: bool,
    /// The tab strip's `+` button's painted bounds, captured every render (same `gpui::canvas`
    /// pattern as [`Self::body_bounds`]). [`Self::render_plus_menu`] positions the popover
    /// directly off this rather than a second, independently-computed offset that could drift
    /// once the rail's adjustable width shifts the button. `Bounds::default()` until first paint.
    /// Only the real anchor when [`Self::plus_menu_repo_anchor`] is `None` - see that field's own
    /// docs for the rail's per-repo `+` case.
    pub(crate) plus_button_bounds: gpui::Bounds<Pixels>,
    /// Which control opened [`Self::plus_menu_open`]'s popover: `None` for the tab strip's own
    /// `+` ([`Self::plus_button_bounds`]), `Some(repo_id)` for that repo's own header `+`
    /// ([`crate::rail::render::AdeApp::render_repo_group_new_button`],
    /// [`Self::rail_plus_button_bounds`]). GitHub issue #113's per-repo `+` used to always
    /// position the popover off [`Self::plus_button_bounds`] regardless of which button actually
    /// opened it - visually anchored to the tab strip even when a rail row's own `+` was clicked.
    /// Set on every real click of either button (see both render sites), read only by
    /// [`Self::render_plus_menu`].
    pub(crate) plus_menu_repo_anchor: Option<RepoId>,
    /// Each rail repo header's own `+` button's painted bounds, captured every render the same
    /// `gpui::canvas` idiom [`Self::plus_button_bounds`] uses - keyed by [`RepoId`] since, unlike
    /// the tab strip's single `+`, one such button paints per repo group every frame regardless
    /// of which one (if any) was actually clicked; a single shared field would silently hold
    /// whichever repo happened to render last. [`Self::render_plus_menu`] looks up the entry for
    /// [`Self::plus_menu_repo_anchor`] when it is `Some`. No entry until that repo's header has
    /// painted at least once.
    pub(crate) rail_plus_button_bounds: std::collections::HashMap<RepoId, gpui::Bounds<Pixels>>,
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
    /// agent instead of the second cancelling the first's still-in-flight search.
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
    /// Each worktree's own real, drag-chosen tab order (GitHub issue #16) - agent and file
    /// tabs interleaved, keyed by that worktree's cwd. Never itself the source of truth for
    /// which tabs exist (`Agents`/[`Self::open_files`] still are - see
    /// [`Self::combined_tab_order`]'s own docs); only [`Self::reorder_tab`] writes to it. A
    /// worktree with no entry here yet falls back to [`Self::tab_order_state`]'s real, persisted
    /// order (`crate::work_surface::tab_order_state::TabOrderState::file_order`) rather than the old
    /// two-block "agents then files" layout - see [`Self::combined_tab_order`]'s own docs for
    /// exactly where that fallback happens.
    pub(crate) tab_order: HashMap<PathBuf, Vec<work_surface::TabRef>>,
    /// The tab strip's real, on-disk drag order (GitHub issue #16: "the resulting layout...
    /// persists per session/worktree and restores on relaunch") - loaded once at startup
    /// (`Self::new_with_settings`), updated by [`Self::reorder_tab`] alongside [`Self::tab_order`]
    /// itself, and read by [`Self::combined_tab_order`] as the fallback for a worktree
    /// [`Self::tab_order`] hasn't touched yet this session.
    pub(crate) tab_order_state: crate::work_surface::tab_order_state::TabOrderState,
    /// [`Self::tab_order_state`]'s resolved on-disk path
    /// (`crate::work_surface::tab_order_state::tab_order_path_for`), `None` for the same tests that get
    /// no [`Self::fold_state_path`] either - see that field's own docs.
    pub(crate) tab_order_path: Option<PathBuf>,
    /// The `crate::work_surface::tab_order_state::worktree_key`s this instance has recorded a real order
    /// for - what [`Self::persist_tab_order`] hands `TabOrderState::save_merged_at` as "mine to
    /// overwrite", mirroring [`Self::fold_state_owned`]'s own reasoning exactly.
    pub(crate) tab_order_owned: std::collections::BTreeSet<String>,
    /// The in-flight background save task from the most recent [`Self::persist_tab_order`] call -
    /// held so it isn't dropped (and therefore cancelled) before it finishes. A single slot: a
    /// tab reorder is a discrete, human-paced drag-drop gesture, not a hot loop, so unlike
    /// [`Self::fold_state_save_pending`]'s coalescing queue, a new reorder simply starts a fresh
    /// save rather than needing one to be pending while another runs.
    pub(crate) _tab_order_save_task: Option<Task<()>>,
    /// The unified tab strip's real, precise drop-target indicator (GitHub issue #16's "better
    /// visual feedback" ask): `Some((target, insert_after))` while a tab is being dragged over
    /// `target`'s own tab, where `insert_after` says whether the cursor is over the right half
    /// of `target` (dragged tab would land immediately after it) or the left half (immediately
    /// before) - see [`Self::render_tab_strip`]'s per-tab `on_drag_move` wiring. Cleared on drop
    /// and, defensively, on any mouse-up over the workspace body, so a cancelled drag (released
    /// outside any drop target) can't leave a stale caret painted on a tab no drag is over
    /// anymore.
    pub(crate) tab_drag_insertion: Option<(work_surface::TabRef, bool)>,
    /// The unified tab strip's real drag-ghost state (GitHub issue #16's "the original slot
    /// renders dimmed" ask): `Some(tab_ref)` for exactly the tab currently being dragged, from
    /// its own `on_drag` constructor callback (`Self::render_file_tab`/`Self::render_agent_tab`)
    /// until either a real drop (`Self::drop_dragged_tab`) or a cancelled drag (the same
    /// workspace-body `on_mouse_up` that clears [`Self::tab_drag_insertion`] on a cancel clears
    /// this too). Never two tabs at once: a new drag's own `on_drag` overwrites whatever was
    /// here, which only matters if a previous drag's cancel-cleanup somehow missed - a defensive
    /// property, not a real multi-drag scenario GPUI itself allows.
    pub(crate) dragging_tab: Option<work_surface::TabRef>,
    /// A real drop's own settle-in animation (GitHub issue #16's "dropping animates the tab
    /// settling into its slot") - `Some((tab, id))` for the tab [`Self::drop_dragged_tab`] most
    /// recently placed, `id` a fresh [`Self::next_tab_settle_id`] value so
    /// `Self::render_file_tab`/`Self::render_agent_tab`'s own `gpui::AnimationExt::with_animation`
    /// call always restarts rather than reusing GPUI's per-element-id animation state from a
    /// previous drop of the very same tab (GPUI keys that state purely off the id string - see
    /// `vendor/zed/crates/gpui/src/elements/animation.rs`'s own `AnimationState`). Left set after
    /// the animation naturally finishes rather than explicitly cleared: a finished one-shot
    /// `Animation` just keeps resolving to its own end state (full opacity) on every later
    /// render, so a stale value here is harmless, and GPUI itself stops scheduling animation
    /// frames for it once done (`AnimationElement::request_layout`'s own `if !done {
    /// request_animation_frame() }`).
    pub(crate) dropped_tab_settle: Option<(work_surface::TabRef, u64)>,
    /// The next fresh id [`Self::drop_dragged_tab`] will stamp into [`Self::dropped_tab_settle`] -
    /// see that field's own docs for why a fresh id matters every time.
    pub(crate) next_tab_settle_id: u64,
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
    /// GitHub issue #141's "Import VSCode theme..." real native file-picker task
    /// (`Self::start_import_vscode_theme`) - same one-slot reasoning as
    /// [`Self::_custom_theme_import_task`], a separate field since a plain-TOML import and a
    /// VSCode-JSON import are two independent real actions/dialogs.
    pub(crate) _vscode_theme_import_task: Option<Task<()>>,
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
    /// `Self::discard_confirm_armed`). `Self::request_remove_custom_theme`
    /// is the one real place this is armed/consumed - a first click on a given name arms it, a
    /// second click on the *same* name actually deletes. Disarmed by leaving the Themes settings
    /// page or reopening Settings (`Self::select_settings_page`/`Self::open_settings`), the same
    /// "most other gestures clear it" discipline `Self::discard_confirm_armed`'s own docs
    /// describe, scoped to this control's own page since nothing else in the app can arm it.
    pub(crate) custom_theme_remove_armed: Option<String>,
    /// The Themes page's most recent icon-pack action result (GitHub issue #5's "custom icon
    /// packs") - `Ok` message or a real, honest `Err` one, shown as an inline status line until
    /// the next action replaces it. Mirrors [`Self::custom_theme_status`]'s own shape, kept as a
    /// separate field since choosing/clearing an icon pack and importing/exporting a theme are
    /// two independent real actions that must never overwrite each other's own status line.
    pub(crate) icon_pack_status: Option<Result<String, String>>,
    /// The in-flight "Choose folder..." real native directory-picker task
    /// (`Self::start_choose_icon_pack_folder`) - a single slot, matching
    /// [`Self::_custom_theme_import_task`]'s own one-dialog-at-a-time reasoning.
    pub(crate) _icon_pack_choose_task: Option<Task<()>>,
    /// The in-flight "Open Folder…" real native directory-picker task (GitHub issue #90,
    /// `Self::start_choose_repo_folder`, wired to the File menu's own row -
    /// `crate::title_bar::menu::AdeApp::file_menu_rows`) - a single slot, matching
    /// [`Self::_icon_pack_choose_task`]'s own one-dialog-at-a-time reasoning.
    pub(crate) _repo_folder_choose_task: Option<Task<()>>,
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

    /// The real [`crate::code_surface::code_view::HighlightOptions`] every production highlight in
    /// this app runs under - the one place a user's own syntax-highlighting preferences are turned
    /// into the value the pipeline consumes, so no call site reads
    /// `self.settings.appearance.*` for this and risks disagreeing with another.
    ///
    /// Read on the foreground thread and passed **by value** into whatever background work needs
    /// it (`spawn_file_load`, `schedule_rehighlight`). It deliberately isn't ambient state: a
    /// `thread_local!` (the `theme::CURRENT_THEME` pattern) would be invisible to the background
    /// executor these highlights actually run on, and a process-global was already tried and
    /// reverted in this codebase for the parallel-test flakes it caused - see `CURRENT_THEME`'s
    /// own docs.
    pub(crate) fn highlight_options(&self) -> crate::code_surface::code_view::HighlightOptions {
        crate::code_surface::code_view::HighlightOptions {
            bracket_pair_colorization: self.settings.appearance.bracket_pair_colorization,
        }
    }

    /// Drops every cached syntax-highlighting result and re-derives it, so a settings change that
    /// alters *span production* rather than paint-time colour really takes effect on already-open
    /// content instead of only on the next file opened.
    ///
    /// Needed because [`Self::highlight_options`] feeds
    /// `crate::code_surface::code_view::HighlightOptions`, whose output is baked into
    /// `RenderedLine` runs and then cached in four independent places - none of which key their
    /// freshness on settings (the File view's on `(path, mtime, len)`, the Diff view's on whole-
    /// `DiffFile` equality, the Merge view's on `(path, hunk)`, and each `EditBuffer`'s on its own
    /// `highlight_dirty` flag). Nulling a cache is not enough on its own for the Diff and Merge
    /// ones: their `ensure_*` methods early-return on an equality check, so each has to be cleared
    /// *and* re-driven. The Markdown preview needs nothing here - it re-renders from source every
    /// frame and so picks the change up for free.
    ///
    /// Every open [`edit_buffer::EditBuffer`] is marked dirty and re-highlighted through the same
    /// debounced background path typing already uses, rather than a synchronous foreground parse
    /// of every open file - see `crate::code_surface::code_view`'s own "Re-highlighting cost"
    /// notes for why a foreground parse of a large file is the thing to avoid.
    pub(crate) fn invalidate_syntax_highlighting(&mut self, cx: &mut Context<Self>) {
        self.file_view_cache = None;
        self.file_view_last_freshness_check = None;

        self.diff_highlight_cache = None;
        self.ensure_diff_highlight_cache();

        self.merge_highlight_cache = None;
        self.ensure_active_merge_highlight_cache();

        // Keyed by `(cwd, path)`; `schedule_rehighlight` resolves against the *current*
        // `file_tree_root`, so only buffers belonging to it can be re-driven here. Buffers from
        // another worktree are still marked dirty, so they re-highlight the moment that worktree
        // is selected again rather than being silently left stale.
        let cwd = self.file_tree_root.clone();
        let mut to_rehighlight = Vec::new();
        for ((buffer_cwd, path), buffer) in self.edit_buffers.iter_mut() {
            buffer.highlight_dirty = true;
            if buffer_cwd == &cwd {
                to_rehighlight.push(path.clone());
            }
        }
        for path in to_rehighlight {
            self.schedule_rehighlight(path, cx);
        }
        cx.notify();
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

    /// The repo currently focused for every single-repo-scoped piece of state ([`Self::
    /// focused_repo_path`], and - for now - [`Self::worktrees`]/[`Self::file_tree_root`]/
    /// [`Self::diff_root`] alike) - `None` only when [`Self::repos`] is empty.
    pub(crate) fn focused_repo(&self) -> Option<&Repo> {
        self.focused_repo
            .and_then(|id| self.repos.iter().find(|repo| repo.id == id))
    }

    /// [`Self::focused_repo`]'s path, or an empty [`PathBuf`] when nothing is focused at all
    /// (GitHub issue #90's genuinely empty window, or a stale [`Self::focused_repo`] id no longer
    /// in [`Self::repos`] - defensive, not reachable through any real mutator today).
    ///
    /// Deliberately **not** `self.repos.first()` - an independent audit found this exact fallback
    /// was a real, reachable bug: [`Self::repos`] is populated from the *whole* persisted
    /// `repos.toml` (every repo this user has ever opened, in any window), not just the one this
    /// window has focused, so falling back to its first entry could silently resolve to a
    /// completely different, unopened repo's real path - and every real repo-scoped operation
    /// this app has (spawning an agent, committing, discarding a worktree) reads its target
    /// through this method or [`Self::active_agent_cwd`]. Every call site that can run with no
    /// repo focused must check [`Self::focused_repo`]/[`Self::focused_repo_path`]'s emptiness
    /// itself instead - see [`crate::work_surface::render::AdeApp::new_agent`]'s own docs for the
    /// concrete exploit this was found through and the real guard that closes it.
    pub(crate) fn focused_repo_path(&self) -> PathBuf {
        self.focused_repo()
            .map(|repo| repo.path.clone())
            .unwrap_or_default()
    }

    /// Adds `path` as a repo the user has open, or returns the id of an already-added repo at the
    /// same real (canonicalized) path - adding the same repo twice (e.g. two `app <path>`
    /// launches sharing one `~/.config/jerry`, or a redundant call from a future "add repo" UI)
    /// is idempotent, never a duplicate rail group. Does **not** change [`Self::focused_repo`] -
    /// see [`Self::focus_repo`] for that half, kept as a separate call so a caller that only
    /// wants "make sure this repo is known" (startup, before deciding what's focused) doesn't get
    /// an unwanted focus side effect.
    ///
    /// Calls [`repo::repo_key`] synchronously (a single `std::fs::canonicalize`, not offloaded to
    /// the background executor) - safe here the same way [`Settings::load_or_init`]'s own
    /// single-file-read constructor exception is safe (see [`Self::new`]'s docs): this only ever
    /// runs from a real, infrequent user action or once at startup, never a render or per-
    /// keystroke path, unlike [`repo::repo_key`]'s hot-path sibling
    /// `crate::sidebar::fold_state::worktree_key`.
    pub(crate) fn add_repo(&mut self, path: PathBuf, cx: &mut Context<Self>) -> RepoId {
        let key = repo::repo_key(&path);
        if let Some(key) = key.as_deref() {
            if let Some(existing) = self
                .repos
                .iter()
                .find(|repo| repo::repo_key(&repo.path).as_deref() == Some(key))
            {
                return existing.id;
            }
        }
        let id = RepoId(self.next_repo_id);
        self.next_repo_id += 1;
        self.repos.push(Repo::new(id, path));
        if let Some(key) = key {
            self.repo_state_owned.insert(key);
        }
        self.persist_repo_state(cx);
        cx.notify();
        id
    }

    /// Makes `id` [`Self::focused_repo`] - a no-op if `id` isn't (or is no longer) in
    /// [`Self::repos`], so a stale id from a closed-over click handler can never point focus at
    /// nothing. Deliberately synchronous and cheap: unlike [`Self::select_worktree`], changing
    /// which *repo* is focused doesn't yet reload anything (the file tree/diff/agents are still
    /// single-repo-scoped fields this phase doesn't rewire - see [`Self::worktrees`]'s own docs),
    /// so there is nothing to kick off here beyond the assignment itself.
    ///
    /// GitHub issue #90: also persists this as [`repo::RepoState::last_focused`] (via
    /// [`Self::persist_repo_state`], the same real writer [`Self::add_repo`] already calls) - "the
    /// app remembers the last-opened folder and reopens it automatically next launch" needs every
    /// real focus change recorded, not just the one a fresh repo's own `add_repo` call happens to
    /// trigger. A no-op call (unknown `id`) persists nothing new, since [`Self::focused_repo`]
    /// never actually changed.
    pub(crate) fn focus_repo(&mut self, id: RepoId, cx: &mut Context<Self>) {
        if self.repos.iter().any(|repo| repo.id == id) {
            self.focused_repo = Some(id);
            self.persist_repo_state(cx);
        }
    }

    /// GitHub issue #90's "Open Folder…" (`crate::title_bar::menu`'s File-menu row) and the
    /// empty-state view's own "Open Folder" button (`Self::render_empty_state`) both funnel
    /// through here: `path` becomes a real, focused repo in the *current* window, and every
    /// single-repo-scoped piece of state this app still has is reset/reloaded against it via
    /// [`Self::reset_repo_scoped_state`] (the exact same reset [`Self::select_worktree`] applies
    /// on every worktree switch, extracted so this real "switch to an entirely different repo"
    /// gesture can share it rather than reimplementing a partial copy) - unlike plain
    /// [`Self::add_repo`] + [`Self::focus_repo`] alone, which (per their own docs) don't reload
    /// anything, since until now the only place that combination ran was startup, where the
    /// caller went on to do the reload itself right afterwards ([`Self::new_with_settings`]).
    ///
    /// An independent audit of this method's first version found (and this now fixes) three real
    /// gaps beyond the incomplete state reset above:
    /// - it never (re)started [`Self::start_worktree_watch`]/[`Self::start_status_polling`] - a
    ///   window that starts empty and only later opens a folder never got rail status polling at
    ///   all, and a window switching from repo A to repo B kept its watcher bound to A's path.
    ///   Both are safe to call unconditionally here: assigning a fresh `Task`/watcher to their own
    ///   field drops (and so cancels) whatever was previously running there, so this can never
    ///   leave two loops running at once.
    /// - a genuinely empty window's Settings/palette overlay may have captured
    ///   [`Self::empty_state_focus_handle`] as its own "return focus to" target
    ///   ([`OverlayFocus::capture`]) - once a real repo is focused, [`Self::render_empty_state`]
    ///   stops being part of the rendered tree at all, so restoring focus to it later would
    ///   silently dangle every global keybinding. [`OverlayFocus::forget_target`] is this
    ///   project's own established fix for exactly this class of bug (see its own docs).
    /// - every agent open before this call belonged to whatever repo (or the empty state) was
    ///   focused *before* it - this app's worktree list/tab-strip filtering is still single-
    ///   repo-scoped (see [`Self::repos`]' own docs: nothing yet renders more than the focused
    ///   repo's own worktrees), so once focus moves to `path` none of them can be reached through
    ///   this window's UI ever again. A deliberate choice, not an accidental process leak: shut
    ///   every one of them down cleanly via the same real PTY teardown [`Self::close_agent`]
    ///   always uses, rather than leaving their processes running invisibly until the whole
    ///   window closes. Skipped entirely when `path` resolves to the repo *already* focused (a
    ///   plain re-affirmation, not a real switch) - closing and immediately not respawning that
    ///   exact repo's own agents would be a real, surprising regression for that case.
    ///
    /// Idempotent against a repo already known to this window (re-opening an already-added repo
    /// just re-focuses and reloads it, rather than duplicating anything) - the same
    /// [`Self::add_repo`] guarantee this builds on. A repo with no agent open yet in its own root
    /// (a genuinely new repo, or one added but never focused before - by construction, always the
    /// case whenever `path` differs from what was focused before, now that stale agents are
    /// closed above) gets one spawned here, the same "a fresh window starts with one shell in the
    /// repo root" default [`Self::new_with_settings`] gives a CLI-launched repo.
    pub(crate) fn open_repo_in_current_window(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previously_focused_id = self.focused_repo().map(|repo| repo.id);
        let id = self.add_repo(path.clone(), cx);
        let switching_repos = previously_focused_id != Some(id);
        self.focus_repo(id, cx);

        self.settings_focus
            .forget_target(&self.empty_state_focus_handle);
        self.palette_focus
            .forget_target(&self.empty_state_focus_handle);

        if switching_repos {
            let stale_ids: Vec<AgentId> = self.agents.iter().map(|agent| agent.id).collect();
            for stale_id in stale_ids {
                self.close_agent(stale_id, window, cx);
            }
        }

        if self.agents.iter_for_cwd(path.clone()).next().is_none() {
            self.agents.spawn(
                AgentKind::Shell,
                path.clone(),
                self.settings.appearance.terminal_font_size,
                window,
                cx,
            );
        }
        self.agents.activate_for_worktree(&path, cx);
        self.selected = None;
        self.worktree_selection_notice = None;
        self.reset_repo_scoped_state(path, window, cx);
        self.load_worktrees(cx);
        self.start_worktree_watch(cx);
        self.start_status_polling(cx);
    }

    /// GitHub issue #113's "click a repo header in the rail, even one with zero open
    /// worktrees, and it checks out" - the rail-native sibling of
    /// [`Self::open_repo_in_current_window`]. Shares that method's real repo-switch reload
    /// (`Self::reset_repo_scoped_state`, `Self::start_worktree_watch`/`Self::start_status_polling`,
    /// forgetting a dangling [`Self::empty_state_focus_handle`] overlay target) but deliberately
    /// does **not** call [`Self::add_repo`] or spawn an initial shell: `id` must already be a
    /// known [`Self::repos`] entry (every row the rail renders came from there), and unlike "Open
    /// Folder…" - which always guarantees *some* real terminal is running so a freshly opened
    /// folder is never inert - this gesture's whole point is to make a genuinely empty repo (zero
    /// open worktrees/agents) reachable as a real "focused, nothing open yet" state, so the user
    /// can choose what to open next themselves (the tab strip's own `+` menu, or the rail's own
    /// per-repo `+` - [`crate::rail::render::AdeApp::render_repo_group`]) instead of always
    /// landing in an unwanted shell.
    ///
    /// A no-op if `id` isn't (or is no longer) a known repo - the same defensive guard
    /// [`Self::focus_repo`] already has, reused here rather than duplicated - or if `id` is
    /// already [`Self::focused_repo`] (a plain re-click of the repo already showing must not
    /// reset any of its live state).
    ///
    /// Unlike [`Self::open_repo_in_current_window`], this never spawns a fallback shell: `id`'s
    /// repo starts genuinely empty (no agents) right after this call, exactly the "focused,
    /// nothing open yet" state this method's own module docs above describe - the user opens
    /// something next via the tab strip's own `+` or the rail's per-repo `+`
    /// ([`crate::rail::render::AdeApp::render_repo_group_new_button`]). Every agent that was open
    /// before this call belongs to whatever repo was focused *before* `id` (this app's tab strip
    /// is still single-repo-scoped - see [`Self::repos`]'s own docs), and all of them are closed
    /// a few lines below via the same real PTY teardown [`Self::close_agent`] always uses, the
    /// identical real fix [`Self::open_repo_in_current_window`] already applies. There is no
    /// cross-restart persistence of *which tabs were open* for a repo yet - a real, disclosed gap,
    /// not a silently stubbed one.
    pub(crate) fn checkout_repo_from_rail(
        &mut self,
        id: RepoId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focused_repo().map(|repo| repo.id) == Some(id) {
            return;
        }
        let Some(path) = self
            .repos
            .iter()
            .find(|repo| repo.id == id)
            .map(|repo| repo.path.clone())
        else {
            return;
        };

        self.focus_repo(id, cx);

        self.settings_focus
            .forget_target(&self.empty_state_focus_handle);
        self.palette_focus
            .forget_target(&self.empty_state_focus_handle);

        // See this method's own docs: every currently open agent belongs to whatever repo was
        // focused before this call (this app's tab strip is still single-repo-scoped), so all of
        // them become permanently unreachable through this window's UI the instant focus moves
        // to `id` - shut them down cleanly rather than leaking their real PTY processes, the
        // identical real fix `Self::open_repo_in_current_window` already applies.
        let stale_ids: Vec<AgentId> = self.agents.iter().map(|agent| agent.id).collect();
        for stale_id in stale_ids {
            self.close_agent(stale_id, window, cx);
        }

        // No `Agents::activate_for_worktree(&path, ...)` call here (unlike
        // `Self::open_repo_in_current_window`, right after its own equivalent teardown loop
        // above): `id` wasn't the focused repo before this call (checked at the top), and this
        // app's tab strip is single-repo-scoped, so no agent can already have `cwd == path` at
        // this point - the loop just above closed every agent that was open, and none of them
        // belonged to `path` to begin with. Calling it would resolve to `active = None`, a real
        // no-op - see this method's own docs above for why `id`'s repo is meant to come up
        // genuinely empty here rather than restoring anything.
        self.selected = None;
        self.worktree_selection_notice = None;
        self.reset_repo_scoped_state(path, window, cx);
        self.load_worktrees(cx);
        self.start_worktree_watch(cx);
        self.start_status_polling(cx);
    }

    /// GitHub issue #90's real, native "Open Folder…" picker - `gpui::App::prompt_for_paths`
    /// (`directories: true`), the identical real API `crate::settings::render::AdeApp::
    /// start_choose_icon_pack_folder` already uses for the same reason (a native OS directory
    /// dialog, not an in-app fake one). On a real chosen directory, hands it to
    /// [`Self::open_repo_in_current_window`] - via `this.update_in`, not a plain `this.update`,
    /// since that reload needs a real `&mut Window` (moving keyboard focus onto whatever agent
    /// ends up active) that a background task resuming after this dialog's own `.await` doesn't
    /// otherwise have.
    pub(crate) fn start_choose_repo_folder(&mut self, cx: &mut Context<Self>) {
        // Guards against a real, reachable re-entry bug an independent audit found: a second
        // click (the File menu's own row, or the empty-state view's button) while a real native
        // picker from the *first* click is still open would otherwise replace
        // `_repo_folder_choose_task`, dropping (and so cancelling, per GPUI's "dropping a `Task`
        // cancels it" semantics - `crate::work_surface::agents::Agents::spawn`'s own docs use the
        // identical reasoning) the first click's still-pending picker task out from under the
        // dialog the user is actually looking at. A no-op second click here is the honest fix -
        // exactly one native "Open" dialog can be in flight from this app at a time. The spawned
        // task below clears this same field back to `None` the instant the dialog resolves
        // (whichever way), so this guard only ever blocks a real second click while one is
        // genuinely still open - never every click after the first.
        if self._repo_folder_choose_task.is_some() {
            return;
        }
        let paths_receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        let task = cx.spawn(async move |this, cx| {
            let result = paths_receiver.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this._repo_folder_choose_task = None;
                let Ok(Ok(Some(mut paths))) = result else {
                    return;
                };
                let Some(path) = paths.pop() else {
                    return;
                };
                this.open_repo_in_current_window(path, window, cx);
            });
        });
        self._repo_folder_choose_task = Some(task);
    }

    /// [`Self::repos`], reduced to the on-disk shape [`repo::RepoState::save_merged_at`] expects -
    /// every repo whose path resolves to a real [`repo::repo_key`] (a non-UTF-8 path is silently
    /// omitted here, the identical refusal `crate::sidebar::fold_state::FoldState`'s own key
    /// functions apply, rather than stored under a lossily-mangled key that could collide).
    fn repo_state_snapshot(&self) -> repo::RepoState {
        let mut state = repo::RepoState::default();
        for r in &self.repos {
            if let Some(key) = repo::repo_key(&r.path) {
                state.repos.insert(
                    key,
                    repo::RepoRecord {
                        name: r.name.clone(),
                    },
                );
            }
        }
        // GitHub issue #90: the real "last opened folder" this instance currently believes -
        // `None` only when nothing is focused at all (a genuinely empty window that has never
        // opened a repo, e.g. from `crate::title_bar::menu`'s "New Window" row), which
        // `RepoState::save_merged_at`'s own docs already explain is deliberately never persisted
        // over another process's real remembered value.
        state.last_focused = self.focused_repo().and_then(|r| repo::repo_key(&r.path));
        state
    }

    /// Queues a background-executor save of the current repo list to [`Self::repo_state_path`],
    /// called from [`Self::add_repo`] (and, once a later phase adds a way to remove one, would be
    /// called from there too - [`Self::repo_state_owned`]/[`repo::RepoState::save_merged_at`]
    /// already carry a real deletion-on-absence contract, unused only for lack of a caller).
    /// `None` path (every GPUI test that hasn't asked for a real one) makes this a no-op; a save
    /// failure is logged, not surfaced - the same simpler shape [`Self::persist_settings`] uses (a
    /// single retry-free attempt per pending write), not [`Self::persist_fold_state`]'s elaborate
    /// bounded-retry loop: losing one `repos.toml` write is a rare, low-stakes miss a user would
    /// notice and just re-add the repo for, unlike a silently-dropped file-tree expand/collapse
    /// edit that issue #18 specifically called out as needing to survive a transient failure.
    pub(crate) fn persist_repo_state(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.repo_state_path.clone() else {
            return;
        };
        self.repo_state_save_pending = true;
        if self.repo_state_save_running {
            // The loop below already re-checks `repo_state_save_pending` before writing or
            // stopping - see `Self::persist_settings`'s identical comment for why spawning a
            // second loop here would let two real `save_merged_at` calls overlap.
            return;
        }
        self.repo_state_save_running = true;
        let task = cx.spawn(async move |this, cx| loop {
            let step = this.update(cx, |this, _cx| {
                if this.repo_state_save_pending {
                    this.repo_state_save_pending = false;
                    Some((this.repo_state_snapshot(), this.repo_state_owned.clone()))
                } else {
                    this.repo_state_save_running = false;
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
            if let Err(err) = result {
                log::warn!("failed to save {}: {err}", path.display());
            }
        });
        self._repo_state_save_task = Some(task);
    }
}

impl Render for AdeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            // (`contexts.last()` is `None`) - live-reproduced while wiring up `CloseFocusedTab`'s
            // `Some("!terminal")` scoping: with Settings open (a real focus target with no
            // `.key_context(..)` anywhere on its own ancestor chain), the stack was genuinely
            // empty, so `!terminal` never got a chance to evaluate "is 'terminal' absent" - it
            // just always returned `false` (never matching) regardless of whether a terminal
            // was anywhere in sight. `"app"` here guarantees the stack always has at least one
            // frame, so `!terminal` (and any future negated-context predicate) evaluates its
            // real, intended logic everywhere.
            .key_context("app")
            // GitHub issue #186's real per-pixel hover tracking, registered once here at the
            // window root rather than per code row. See `AdeApp::track_hover_pointer`'s own docs
            // for why it has to be here: a per-row `.on_mouse_move` only fires while that row is
            // the top-most hitbox under the pointer, so it can never observe the pointer leaving
            // the code area - which is precisely the dismissal case that has to work.
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, _window, cx| {
                    this.track_hover_pointer(event, cx);
                }),
            )
            .on_action(cx.listener(Self::handle_new_agent_action))
            .on_action(cx.listener(Self::handle_toggle_palette_action))
            .on_action(cx.listener(Self::handle_toggle_settings_action))
            .on_action(cx.listener(Self::handle_goto_definition_action))
            .on_action(cx.listener(Self::handle_new_terminal_action))
            .on_action(cx.listener(Self::handle_new_agent_pane_action))
            .on_action(cx.listener(Self::handle_new_git_graph_action))
            .on_action(cx.listener(Self::handle_next_changed_file_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_1_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_2_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_3_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_4_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_5_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_6_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_7_action))
            .on_action(cx.listener(Self::handle_jump_to_agent_8_action))
            .on_action(cx.listener(Self::handle_close_focused_tab_action))
            .on_action(cx.listener(Self::handle_terminal_clear_action))
            .on_action(cx.listener(Self::handle_terminal_copy_action))
            .on_action(cx.listener(Self::handle_terminal_paste_action))
            .child(self.render_title_bar(cx))
            // The Settings surface (`design_handoff_jerry_ade/README.md`: "a separate surface,
            // not a modal: it replaces the three zones while the title bar and status bar
            // stay") swaps out only this one child - the title bar above and the status bar
            // below are unconditional siblings, rendered every frame regardless of
            // `settings_open`.
            .child(if self.settings_open {
                self.render_settings(cx).into_any_element()
            } else if self.focused_repo().is_none() {
                // GitHub issue #90: a genuinely empty window (no CLI argument, nothing ever
                // persisted/remembered, or `crate::title_bar::menu`'s own "New Window" row) - see
                // `Self::render_empty_state`'s own docs. Checked before `render_workspace_body`
                // because that method (and everything it renders - the rail, tab strip, file
                // tree) assumes a real focused repo throughout; this is deliberately *not*
                // `self.repos.is_empty()`, since a "New Window" can genuinely have known-but-
                // unfocused repos in that list (loaded from the same persisted `repos.toml`
                // every window in this process shares) while still wanting its own empty view.
                self.render_empty_state(cx).into_any_element()
            } else {
                self.render_workspace_body(cx).into_any_element()
            })
            .child(self.render_status_bar(cx))
            .when(self.plus_menu_open, |el| {
                el.child(self.render_plus_menu(cx))
            })
            // The git graph tab's Push `▾` menu and row `⋯` menu (GitHub issue #1, phase (a)) -
            // window-positioned overlays for the same reason `render_plus_menu` is a sibling here
            // rather than nested inside the workspace body: their `gpui::canvas`-captured bounds
            // are window-space, so `.absolute()` positioning built from them is only correct as a
            // direct child of this root element (a real, adversarial-audit-found bug when they
            // were nested inside `crate::graph_view::render::AdeApp::render_graph_view`'s own
            // container - see that method's docs).
            //
            // Also gated on `!self.settings_open`, the same belt-to-`Self::open_settings`'s-
            // braces reasoning the tree context menu gate below documents: opening Settings does
            // not clear `graph_tab_active` (the graph tab underneath is still "active"), and
            // `open_settings` already clears both `graph_state.row_menu_open`/`push_menu_open`
            // itself now (an adversarial-audit-found gap, fixed there) - this is defensive
            // padding against that invariant ever drifting, not the primary fix.
            .when(
                self.graph_tab_active && !self.settings_open && self.graph_state.push_menu_open,
                |el| el.child(self.render_graph_push_menu(cx)),
            )
            .when(
                self.graph_tab_active
                    && !self.settings_open
                    && self.graph_state.row_menu_open.is_some(),
                |el| el.child(self.render_graph_row_menu(cx)),
            )
            .when_some(self.title_menu_open, |el, menu| {
                el.child(self.render_title_menu(menu, cx))
            })
            // The commit composer's `▾` menu (GitHub issue #176) - a window-positioned overlay for
            // the same reason `render_plus_menu` is one: it is anchored off a `gpui::canvas`-
            // captured, window-space `Self::commit_composer_bounds`, and its click-away scrim has
            // to cover the whole window rather than the composer's own 135px box. See
            // `crate::sidebar::render::AdeApp::render_commit_menu`'s own docs.
            //
            // Gated on the composer really being rendered underneath: Settings replaces the
            // workspace body, the right sidebar may be showing Files instead of Changes, and the
            // Changes view itself falls back to an error message when there is no real diff - in
            // all three cases `commit_composer_bounds` is a stale anchor from an earlier frame.
            // `Self::close_right_sidebar_view`'s own clear keeps `commit_menu_open` from latching
            // across a view switch; this guard is the belt to that braces.
            .when(
                self.commit_menu_open
                    && !self.settings_open
                    && self.right_sidebar_view == RightSidebarView::Changes
                    && self.current_diff().is_some(),
                |el| el.child(self.render_commit_menu(window, cx)),
            )
            .children(self.render_hover_card(window, cx))
            .children(self.render_completions_popover(cx))
            // GitHub issue #186's real Diagnostic popover - a top-level sibling of the other two
            // for exactly the same reason they are (see `render_hover_card`'s own docs), and
            // painted after them so that if the one-at-a-time gate in `render_diagnostic_card`
            // ever failed to hold, the ambient card would be the one on top to notice, not the
            // requested one it would be hiding.
            .children(self.render_diagnostic_card(window, cx))
            .when(self.new_file_input.is_some(), |el| {
                el.child(self.render_new_file_prompt(cx))
            })
            // The file tree's context menu (GitHub issue #19) - a window-positioned overlay, so
            // it lives here beside the `+` menu and the "New file" prompt rather than inside the
            // sidebar's own clipped column. Delete no longer has a confirmation overlay of its
            // own (GitHub issue #105: it runs immediately, reversible via `Self::tree_undo_stack`).
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
            .when(self.palette_open, |el| el.child(self.render_palette(cx)))
    }
}

impl AdeApp {
    /// The three-zone workspace body (agent rail, centre pane, files/changes panel) - pulled
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
            // Defensive cleanup for `Self::tab_drag_insertion`/[`Self::dragging_tab`]: unlike
            // `PaneResizeDrag` above, these *do* need an explicit mouse-up handler, because
            // neither is derived fresh from the cursor position on every tick - they're the last
            // tab strip `on_drag_move::<DraggedTab>`/`on_drag` claimed, which stays stale if the
            // drag ends by releasing outside any tab's own hitbox (a cancelled drag) rather than
            // through a real `on_drop`. The body spans virtually the whole window below the
            // title bar, so this reaches almost every real release point.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &gpui::MouseUpEvent, _window, cx| {
                    if this.cancel_any_tab_drag() {
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

    /// GitHub issue #90's genuinely empty state - rendered by [`Render::render`] in place of
    /// [`Self::render_workspace_body`] whenever [`Self::focused_repo`] is `None`: a fresh launch
    /// with no CLI argument and nothing ever persisted (or a remembered folder that no longer
    /// exists), or a brand-new window opened via `crate::title_bar::menu`'s "New Window" row.
    ///
    /// Deliberately minimal - a centered message plus one real, working affordance - rather than
    /// an inert placeholder: the "Open Folder…" button below calls the exact same
    /// [`Self::start_choose_repo_folder`] real native-picker flow the File menu's own "Open
    /// Folder…" row does (`crate::title_bar::menu::AdeApp::file_menu_rows`), so a user landing
    /// here has a genuine way out of the empty state without needing to already know about the
    /// title bar. The title bar and status bar stay real, unconditional siblings around this (see
    /// [`Render::render`]'s own docs), so Settings/File-menu/Quit are all still reachable from
    /// here exactly as they are from the normal workspace view.
    ///
    /// `track_focus`es [`Self::empty_state_focus_handle`] - the same "never leave `Window::focus`
    /// dangling" reasoning [`Self::rail_focus_handle`]'s own docs give, applied to a window whose
    /// rendered tree has no rail/tab-strip/file-tree at all to fall back onto instead.
    fn render_empty_state(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("empty-state")
            .track_focus(&self.empty_state_focus_handle)
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .bg(theme::surface::WINDOW)
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(14.0))
                    .text_color(theme::text::STRONG)
                    .child("No folder open"),
            )
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.0))
                    .text_color(theme::text::DIM)
                    .child("Open a folder to browse its files, worktrees, and agents."),
            )
            .child(
                div().pt(px(6.0)).child(
                    widgets::render_modal_button(
                        "empty-state-open-folder",
                        "Open Folder\u{2026}",
                        false,
                    )
                    .on_click(cx.listener(
                        |this, _event: &ClickEvent, _window, cx| {
                            this.start_choose_repo_folder(cx);
                        },
                    )),
                ),
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
    /// Which agent was active when [`Self::capture`] ran - compared against the active
    /// agent at restore time so [`restore_focus`] can tell whether `return_focus` is still
    /// safe to restore (it may belong to an agent that's no longer active).
    opened_agent: Option<AgentId>,
}

impl OverlayFocus {
    /// Records the current focus target and active agent. Callers that must only capture on
    /// a genuine closed-to-open transition (not every subsequent navigation while already open -
    /// see [`AdeApp::focus_code_surface`]) guard the call themselves; this always captures
    /// unconditionally when called.
    pub(crate) fn capture(&mut self, window: &Window, agents: &Agents, cx: &App) {
        self.return_focus = window.focused(cx);
        self.opened_agent = agents.active_id();
    }

    /// Discards captured state without restoring it. Three real callers: [`AdeApp::close_palette`]'s
    /// Settings-showing-underneath branch and [`AdeApp::close_palette_keeping_result_focus`],
    /// both of which put focus somewhere real themselves instead of going through
    /// [`restore_focus`], and `crate::work_surface::render`'s agent teardown.
    pub(crate) fn clear(&mut self) {
        self.return_focus = None;
        self.opened_agent = None;
    }

    /// Forgets a captured target that is about to stop being rendered, leaving [`restore_focus`]
    /// to fall back to the active agent's pane instead of focusing a node GPUI can no longer
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
/// If the active agent changed while the surface was open, the captured handle is skipped in
/// favor of the *current* active agent's terminal pane (a handle from a no-longer-active
/// agent would be just as dangling as the overlay's own). Otherwise the captured handle is
/// restored, falling back to the active agent's pane if nothing was focused before. A free
/// function, not an `AdeApp` method, since every caller already holds `&mut self` and needs to
/// pass `&mut self.some_field` alongside it. Deliberately doesn't call `cx.notify()` - every
/// caller has its own surface-specific state change around this call and issues its own single
/// `cx.notify()` once everything, this restore included, is done.
pub(crate) fn restore_focus(
    agents: &Agents,
    overlay_focus: &mut OverlayFocus,
    window: &mut Window,
    cx: &mut App,
) {
    let agent_changed = agents.active_id() != overlay_focus.opened_agent;
    let restore_target = if agent_changed {
        None
    } else {
        overlay_focus.return_focus.take()
    };
    let focus_target =
        restore_target.or_else(|| agents.active().map(|agent| agent.pane.focus_handle(cx)));
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
                Some(repo_path),
                true,
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

/// Real, `Context<AdeApp>`-driven coverage for [`AdeApp::add_repo`]/[`AdeApp::focus_repo`]/
/// [`AdeApp::persist_repo_state`] (Revision R12 Phase 0) - the restructured multi-repo-capable
/// state this revision's rail-rendering/authorship-heuristic/worktree-watcher sibling work
/// depends on. `crate::rail::repo`'s own module carries the pure `RepoState` persistence-format
/// tests (including the deletion-on-absence half `save_merged_at` already supports); these
/// exercise the same behaviour through a real `AdeApp` entity, the way [`settings_persist_tests`]
/// does for settings. No `AdeApp::remove_repo` exists yet - there is no real (non-test) caller
/// for one until a later phase adds a "remove repo" affordance, so it isn't defined here rather
/// than kept as unreachable scaffolding; `Self::repo_state_owned`/`RepoState::save_merged_at`'s
/// contract is already shaped to support it when that phase adds the method and its own tests.
#[cfg(test)]
mod repo_list_tests {
    use super::*;
    use crate::rail::repo::RepoState;
    use gpui::TestAppContext;

    fn open_test_app_with_real_settings_path(
        cx: &mut TestAppContext,
        repo_path: PathBuf,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                Some(repo_path),
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    /// A fresh window's own startup path already exercises the common single-repo case: exactly
    /// one repo, focused, matching the CLI-given path - "a user launching `app <path>` today must
    /// keep working exactly as before" is this test.
    #[gpui::test]
    fn a_fresh_window_starts_with_exactly_one_focused_repo(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.read_with(cx, |app, _| {
            assert_eq!(app.repos.len(), 1);
            assert_eq!(app.repos[0].path, repo.path());
            assert_eq!(app.focused_repo().map(|r| r.id), Some(app.repos[0].id));
            assert_eq!(app.focused_repo_path(), repo.path());
        });
    }

    #[gpui::test]
    fn add_repo_appends_a_new_entry_without_changing_focus(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");
        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        let focused_before = app.read_with(cx, |app, _| app.focused_repo_path());

        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });

        app.read_with(cx, |app, _| {
            assert_eq!(app.repos.len(), 2);
            assert!(app.repos.iter().any(|r| r.path == repo_b.path()));
            assert_eq!(
                app.focused_repo_path(),
                focused_before,
                "add_repo must not change which repo is focused"
            );
        });
    }

    /// Adding the same real path twice (e.g. a repeat `app <path>` launch sharing one
    /// `~/.config/jerry`) must not produce a second rail group for the same repo.
    #[gpui::test]
    fn add_repo_is_idempotent_for_the_same_path(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        let (first_id, second_id) = app.update(cx, |app, cx| {
            let first = app.repos[0].id;
            let second = app.add_repo(repo.path().to_path_buf(), cx);
            (first, second)
        });

        assert_eq!(
            first_id, second_id,
            "re-adding the same path must return the same id"
        );
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.repos.len(),
                1,
                "re-adding the same path must not duplicate it"
            );
        });
    }

    #[gpui::test]
    fn focus_repo_moves_focus_to_a_known_repo(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");
        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        app.update(cx, |app, cx| app.focus_repo(repo_b_id, cx));

        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo_path(), repo_b.path());
        });
    }

    /// A stale/unknown id must not blank out focus - the same "refuse rather than corrupt state"
    /// shape `crate::sidebar::fold_state::FoldState::set_expanded` uses for an out-of-worktree
    /// path.
    #[gpui::test]
    fn focus_repo_with_an_unknown_id_is_a_no_op(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let focused_before = app.read_with(cx, |app, _| app.focused_repo_path());

        app.update(cx, |app, cx| app.focus_repo(RepoId(u64::MAX), cx));

        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo_path(), focused_before);
        });
    }

    /// The persistence half: adding a repo with a real settings path must, once the background
    /// writer loop settles, leave a real `repos.toml` on disk that a fresh load recovers - "which
    /// repos are currently added should survive an app restart".
    #[gpui::test]
    fn adding_a_repo_persists_to_a_real_repos_toml(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo_a.path().to_path_buf(),
            settings_path.clone(),
        );
        app.update(cx, |app, cx| {
            app.add_repo(repo_b.path().to_path_buf(), cx);
        });
        cx.run_until_parked();

        let repo_state_path = repo::repo_state_path_for(&settings_path);
        let on_disk = RepoState::load_at(&repo_state_path);
        let canonical_a = repo::repo_key(repo_a.path()).expect("repo a key");
        let canonical_b = repo::repo_key(repo_b.path()).expect("repo b key");
        assert!(
            on_disk.repos.contains_key(&canonical_a),
            "the startup repo must be persisted too, not just later additions"
        );
        assert!(on_disk.repos.contains_key(&canonical_b));
    }

    /// A window opened against `repo_a`, sharing a real `~/.config/jerry` that *another* running
    /// `jerry` instance already recorded `repo_b` into, must not erase `repo_b` the moment it
    /// saves anything of its own - the identical multi-instance guarantee
    /// `crate::sidebar::fold_state` already provides for the file-tree fold state, proven here
    /// through one real `AdeApp` entity's startup-and-save cycle against a `repos.toml` seeded as
    /// if a second instance had already written to it (`crate::rail::repo`'s own tests already
    /// cover the pure `RepoState::save_merged_at` half; this proves the real `AdeApp` wiring
    /// reaches it correctly end to end).
    #[gpui::test]
    fn opening_against_one_repo_does_not_erase_another_instances_already_persisted_repo(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        // Simulate a second `jerry` instance that already added `repo_b` and saved it.
        let repo_state_path = repo::repo_state_path_for(&settings_path);
        let mut seed = RepoState::default();
        let canonical_b = repo::repo_key(repo_b.path()).expect("repo b key");
        seed.repos.insert(
            canonical_b.clone(),
            crate::rail::repo::RepoRecord {
                name: "repo-b".to_string(),
            },
        );
        seed.save_at(&repo_state_path).expect("seed save");

        // This instance opens against a different repo, sharing the same `repos.toml`.
        let (app, cx) = open_test_app_with_real_settings_path(
            cx,
            repo_a.path().to_path_buf(),
            settings_path.clone(),
        );
        cx.run_until_parked();
        let _ = app;

        let on_disk = RepoState::load_at(&repo_state_path);
        let canonical_a = repo::repo_key(repo_a.path()).expect("repo a key");
        assert!(
            on_disk.repos.contains_key(&canonical_a),
            "this instance's own repo must be persisted"
        );
        assert!(
            on_disk.repos.contains_key(&canonical_b),
            "this instance's own save must not have erased the other instance's already-\
             persisted repo"
        );
    }

    /// GitHub issue #90: a window with no CLI argument (`repo_path: None`) that is allowed to
    /// consult persisted state (`use_remembered_repo: true` - the real process-launch case) and
    /// a fresh settings directory - nothing has ever been persisted - opens in a genuinely empty
    /// state: no repo focused, and none of the single-repo-scoped startup work
    /// (`Self::new_with_settings`'s own docs) ran at all.
    #[gpui::test]
    fn no_cli_arg_and_nothing_persisted_is_a_genuinely_empty_window(cx: &mut TestAppContext) {
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, _cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });

        app.read_with(_cx, |app, _| {
            assert!(app.repos.is_empty(), "nothing was ever added or persisted");
            assert_eq!(app.focused_repo(), None);
            assert!(
                app.agents.is_empty(),
                "a genuinely empty window must not spawn an initial shell agent - there is no \
                 repo root to spawn one into"
            );
        });
    }

    /// GitHub issue #90's own headline behaviour: focusing a repo (via [`AdeApp::focus_repo`])
    /// persists it as [`RepoState::last_focused`], and a *second*, independently-constructed
    /// `AdeApp` sharing the same real settings path, given `repo_path: None,
    /// use_remembered_repo: true` (the real process-launch path with no CLI argument), reopens
    /// that exact repo automatically - "the app remembers the last-opened folder and reopens it
    /// automatically next launch".
    #[gpui::test]
    fn a_fresh_launch_with_no_cli_arg_reopens_the_remembered_last_focused_repo(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        // "Launch 1": a real CLI-argument launch against `repo`, which focuses (and so persists)
        // it.
        let (_first, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_path.clone(),
        );
        cx.run_until_parked();

        // "Launch 2": an independent `AdeApp`, sharing the same real settings path, with no CLI
        // argument at all.
        let (second, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        second.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo_path(),
                repo.path(),
                "the second launch must have reopened the first launch's own remembered repo"
            );
            assert_eq!(app.repos.len(), 1);
            assert!(
                !app.agents.is_empty(),
                "a real, reopened repo must get its usual initial shell agent, exactly like a \
                 real CLI-argument launch does"
            );
        });
    }

    /// The other half of "remembers the last-opened folder": a remembered repo whose directory
    /// has since been deleted or moved must fall back to a genuinely empty window, not a broken
    /// or crashing one.
    #[gpui::test]
    fn a_remembered_repo_that_no_longer_exists_falls_back_to_a_genuinely_empty_window(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let repo_path = repo.path().to_path_buf();
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (_first, cx) =
            open_test_app_with_real_settings_path(cx, repo_path.clone(), settings_path.clone());
        cx.run_until_parked();

        // The remembered repo's own directory is now gone.
        drop(repo);
        assert!(!repo_path.exists());

        let (second, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        second.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo(),
                None,
                "a deleted/moved remembered repo must never be focused"
            );
            assert!(app.agents.is_empty());
        });
    }

    /// GitHub issue #90's "New Window" semantics: `use_remembered_repo: false` (what
    /// `crate::title_bar::menu`'s "New Window" row passes) must open a genuinely empty window
    /// even when a real, still-existing last-focused repo *is* on record - unlike the real
    /// process-launch path (`use_remembered_repo: true`, proven by
    /// [`a_fresh_launch_with_no_cli_arg_reopens_the_remembered_last_focused_repo`] above), which
    /// would reopen this exact repo.
    #[gpui::test]
    fn use_remembered_repo_false_stays_empty_even_with_a_real_remembered_repo(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (_first, cx) = open_test_app_with_real_settings_path(
            cx,
            repo.path().to_path_buf(),
            settings_path.clone(),
        );
        cx.run_until_parked();

        let (second, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                false,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        second.read_with(cx, |app, _| {
            assert_eq!(
                app.focused_repo(),
                None,
                "a 'New Window' must open empty regardless of what's remembered"
            );
            assert!(app.agents.is_empty());
        });
    }

    /// [`AdeApp::open_repo_in_current_window`] - the shared real flow behind both the File menu's
    /// "Open Folder…" row and the empty-state view's own button - genuinely focuses the chosen
    /// repo in an already-running, previously-empty window and reloads its worktrees/file tree/
    /// diff/initial agent, mirroring what a real CLI-argument launch does.
    #[gpui::test]
    fn open_repo_in_current_window_focuses_and_reloads_a_real_repo_from_empty(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("a.txt"), "hello\n").expect("write");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo(), None, "sanity check: starts empty");
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo_path(), repo.path());
            assert!(
                !app.agents.is_empty(),
                "opening a folder must spawn a real initial shell agent for it"
            );
            assert!(
                !app.file_tree.is_empty(),
                "opening a folder must really load its file tree"
            );
        });
    }

    /// Critical fix (independent audit): `Self::open_repo_in_current_window` used to only reset
    /// four fields (`staged_files`/`open_change`/`expanded_dirs`/`selected_tree_path`), leaving
    /// every other per-repo UI control - a tree error banner, the commit composer popover, an
    /// armed prune confirmation, and more - still armed against the *old* repo after switching to
    /// a new one. `Self::reset_repo_scoped_state` (shared with `Self::select_worktree`) now
    /// covers all of it; this proves a representative sample of the fields the original version
    /// missed are really cleared by a real Open Folder switch, not just the original four.
    #[gpui::test]
    fn open_repo_in_current_window_clears_stale_ui_state_from_the_previous_repo(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo_b.path().join("y.txt"), "b\n").expect("write");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        // Arm several pieces of real per-repo UI state against repo A - the same kinds of state
        // `Self::select_worktree` already resets on every worktree switch (GitHub issues #12/#19),
        // which the original `open_repo_in_current_window` did not.
        app.update(cx, |app, cx| {
            app.tree_op_error = Some("stale tree error from repo A".to_string());
            app.commit_menu_open = true;
            app.prune_confirm_armed = true;
            cx.notify();
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo_path(), repo_b.path());
            assert_eq!(
                app.tree_op_error, None,
                "opening a different folder must clear a stale tree error from the old repo"
            );
            assert!(
                !app.commit_menu_open,
                "the old repo's commit composer popover must not stay open"
            );
            assert!(
                !app.prune_confirm_armed,
                "an armed prune confirmation from the old repo must not survive the switch"
            );
        });
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}:\nstdout: {}\nstderr: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// Critical fix (independent audit): the original `open_repo_in_current_window` never
    /// (re)started the real status-polling loop or the worktree filesystem watcher - a window
    /// that starts empty and only later opens a folder never got either at all. `repo` is a real
    /// `git init`-ed directory, not a bare `tempfile::tempdir()`: `spawn_worktree_watcher` returns
    /// `None` for a path that isn't inside a real git repository at all
    /// (`wt_core::git_common_dir` failing), so a bare tempdir would make this test pass "by
    /// accident" - `_worktree_watcher` would read `None` regardless of whether the real fix ever
    /// ran, proving nothing about the watcher half of the fix.
    #[gpui::test]
    fn open_repo_in_current_window_starts_status_polling_and_worktree_watch(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git(repo.path(), &["init", "-b", "main"]);
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        app.read_with(cx, |app, _| {
            assert!(
                app._status_poll_task.is_none(),
                "sanity check: a genuinely empty window never starts real status polling"
            );
            assert!(
                app._worktree_watch_task.is_none(),
                "sanity check: a genuinely empty window never starts the real worktree watcher"
            );
            assert!(app._worktree_watcher.is_none());
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app._status_poll_task.is_some(),
                "opening a folder must start real status polling"
            );
            assert!(
                app._worktree_watch_task.is_some(),
                "opening a folder must start the real worktree watcher's own poll-fallback loop"
            );
            assert!(
                app._worktree_watcher.is_some(),
                "opening a real git repo must really start a real OS-level filesystem watcher \
                 for it, not just the poll-fallback loop"
            );
        });
    }

    /// The other real half of Critical fix 2: a window switching from one real repo to another
    /// must *rebind* the watcher to the new path, not leave it silently still watching the old
    /// one. Proven by making the second repo a bare, non-git directory:
    /// `spawn_worktree_watcher` can only ever return `None` for it
    /// (`wt_core::git_common_dir` fails for a non-repository path) - so `_worktree_watcher`
    /// reading `None` after the switch is genuine proof `Self::start_worktree_watch` really ran
    /// again with repo B's own path, not proof of nothing (a stale watcher still pointed at repo
    /// A - a real git repo - would have kept this field `Some`, silently masking the bug this
    /// test exists to catch).
    #[gpui::test]
    fn open_repo_in_current_window_rebinds_the_worktree_watcher_to_the_new_repo(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        git(repo_a.path(), &["init", "-b", "main"]);
        let repo_b = tempfile::tempdir().expect("tempdir");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                app._worktree_watcher.is_some(),
                "sanity check: repo A is a real git repo, so startup should have gotten a real \
                 watcher for it"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app._worktree_watcher.is_none(),
                "switching to repo B (not a real git repository) must really rebind the watcher \
                 to B's own path and get a real None back for it - a stale watcher left over \
                 from repo A would still read Some here"
            );
        });
    }

    /// Critical fix (independent audit): switching folders used to leave the previous repo's real
    /// agent processes running but permanently unreachable through this window's own UI (this
    /// app's worktree list/tab strip are still single-repo-scoped - see `Self::repos`' own docs).
    /// A deliberate choice, not an accidental leak: `Self::open_repo_in_current_window` now shuts
    /// every one of them down cleanly via the same real PTY teardown `Self::close_agent` always
    /// uses.
    #[gpui::test]
    fn open_repo_in_current_window_closes_the_previous_repos_agents(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                1,
                "sanity check: repo A's own initial shell agent"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents
                    .iter()
                    .filter(|agent| agent.cwd == repo_a.path())
                    .count(),
                0,
                "repo A's own agent must have really been closed, not merely hidden from the UI"
            );
            assert_eq!(
                app.agents.iter().count(),
                1,
                "repo B should have exactly its own fresh initial shell agent"
            );
        });
    }

    /// The rail-native mirror of [`open_repo_in_current_window_closes_the_previous_repos_agents`]
    /// just above - an independent checker audit of GitHub issue #113's fix flagged
    /// [`AdeApp::checkout_repo_from_rail`] as untested destructive one-click teardown: clicking a
    /// rail repo header closes every currently open agent (real PTY teardown, mirroring
    /// [`AdeApp::open_repo_in_current_window`]'s own already-tested behavior above), but a much
    /// cheaper gesture triggers it now - one click on a rail row, versus "File > Open Folder... >
    /// pick a directory" before. Proves two real agents belonging to repo A (its initial shell
    /// plus a spawned Claude session) are both really gone - removed from
    /// [`crate::work_surface::agents::Agents`]'s own list, not merely hidden from the UI - after
    /// checking out repo B from the rail, the same "really closed, not leaked" guarantee the
    /// `open_repo_in_current_window` mirror test proves via the identical technique (asserting
    /// against the live agent list, since [`crate::work_surface::agents::Agents::close`] always
    /// calls `TerminalPane::shutdown` before removing an agent from that list).
    #[gpui::test]
    fn checkout_repo_from_rail_closes_the_previous_repos_agents(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                AgentKind::Claude,
                repo_a.path().to_path_buf(),
                12.0,
                window,
                cx,
            );
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                2,
                "sanity check: repo A's initial shell plus the spawned Claude agent"
            );
        });

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents
                    .iter()
                    .filter(|agent| agent.cwd == repo_a.path())
                    .count(),
                0,
                "repo A's agents must have really been closed (real PTY teardown via \
                 `Agents::close`), not merely hidden from the UI"
            );
            assert_eq!(
                app.agents.iter().count(),
                0,
                "unlike `open_repo_in_current_window`, checking out a repo from the rail must \
                 not auto-spawn a fallback shell - repo B comes up genuinely empty, per \
                 `checkout_repo_from_rail`'s own docs"
            );
            assert_eq!(
                app.focused_repo_path(),
                repo_b.path(),
                "sanity check: repo B really is the focused repo after checkout"
            );
        });
    }

    /// Critical fix (independent audit): a genuinely empty window's Settings overlay can capture
    /// [`AdeApp::empty_state_focus_handle`] as its own "return focus to" target
    /// ([`OverlayFocus::capture`]) - once a real repo is focused, `Self::render_empty_state` stops
    /// being part of the rendered tree at all, so restoring focus there later would silently
    /// dangle every global keybinding. `Self::open_repo_in_current_window` must forget that target
    /// ([`OverlayFocus::forget_target`]) before Settings closes.
    #[gpui::test]
    fn open_repo_in_current_window_forgets_a_dangling_empty_state_focus_target(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });

        let empty_state_handle = app.read_with(cx, |app, _| app.empty_state_focus_handle.clone());
        app.update_in(cx, |app, window, cx| {
            window.focus(&empty_state_handle, cx);
            app.open_settings(window, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.close_settings(window, cx);
        });
        cx.run_until_parked();

        let focused = app.update_in(cx, |_app, window, cx| window.focused(cx));
        assert_ne!(
            focused.as_ref(),
            Some(&empty_state_handle),
            "closing Settings must never restore focus onto the no-longer-rendered empty-state \
             handle"
        );
    }

    /// Critical fix (independent audit): a genuinely empty window has no real repo root to spawn
    /// a new agent into - `Self::focused_repo_path`'s own `Self::repos.first()` fallback (removed)
    /// used to let `Self::active_agent_cwd` silently resolve to some *other*, unopened repo's real
    /// path, so `secondary-n`/`ctrl-shift-T`'s own handlers could spawn a real, invisible PTY
    /// there. `Self::new_agent`/`Self::new_agent_pane` now refuse outright with no focused repo -
    /// this proves both real entry points genuinely spawn nothing.
    #[gpui::test]
    fn new_agent_and_new_agent_pane_are_no_ops_with_no_focused_repo(cx: &mut TestAppContext) {
        let other_repo = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        // A real *other* repo is known to this process (persisted from a previous focus) - the
        // exact precondition the audit's concrete exploit needed (`Self::repos` non-empty while
        // this window's own `Self::focused_repo` is `None`).
        let repo_state_path = repo::repo_state_path_for(&settings_path);
        let mut seed = RepoState::default();
        let canonical = repo::repo_key(other_repo.path()).expect("repo key");
        seed.repos.insert(
            canonical,
            crate::rail::repo::RepoRecord {
                name: "other-repo".to_string(),
            },
        );
        seed.save_at(&repo_state_path).expect("seed save");

        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                None,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo(), None, "sanity check: starts empty");
            assert!(
                !app.repos.is_empty(),
                "sanity check: a real other repo is known, just not focused"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.new_agent(AgentKind::Shell, window, cx);
        });
        app.update_in(cx, |app, window, cx| {
            app.handle_new_agent_pane_action(&NewAgentPane, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.agents.is_empty(),
                "neither real entry point may spawn a PTY with no repo genuinely focused, even \
                 with an unrelated real repo known to this process"
            );
        });
    }
}

pub(crate) mod caret_blink;
pub(crate) mod focus;
pub mod layout;
pub(crate) mod menus;
pub(crate) mod new_file;
pub(crate) mod rem_scope;
pub(crate) mod resize;
pub(crate) mod scrollbar;
pub(crate) mod scrollbar_geometry;
pub(crate) mod state;
pub(crate) mod task_pool;
pub(crate) mod widgets;
