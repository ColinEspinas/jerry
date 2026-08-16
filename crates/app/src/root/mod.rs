//! The top-level three-pane window: a left worktree sidebar, a tabbed center pane of
//! terminal agents, and a right file tree, composed as GPUI entities.

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
use crate::review;
use crate::settings::custom_theme;
use crate::settings::state as settings;
#[cfg(test)]
use crate::settings::state::SettingsPage;
use crate::settings::store::{self as settings_store, CfgFormat, Settings};
use crate::sidebar::changes;
use crate::sidebar::file_tree;
use crate::sidebar::fold_state;
use crate::sidebar::sections;
use crate::sidebar::tree_ops;
use crate::status_bar::process_stats;
use crate::text_history;
use crate::theme;
use crate::title_bar::menu as title_bar;
use crate::title_bar::menu_model::MenuCommand;
use crate::updater;
use crate::work_surface::agents::{AgentId, Agents, ProcessKind};
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
        SearchInWorktree,
        FindInFile,
        NextChangedFile,
        ToggleChangeSeen,
        ToggleChangeStaged,
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
        TextCopy,
        TextCut,
        TextPaste,
        TextSelectAll,
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
        // GitHub issue #235: a real macOS `NSApp.mainMenu`/`gpui::App::set_menus` menu, and the
        // matching macOS application menu (Hide/Hide Others/Show All/Quit) that only a real
        // `NSApp.mainMenu` can host. `crate::title_bar::menu_model::MenuCommand` is the shared
        // source of truth that maps every one of these (plus the already-existing actions it
        // reuses as-is - `EditorSave`, `ToggleSettings`, `TextUndo`, `TextRedo`, `EditorCut`,
        // `EditorCopy`, `EditorPaste`, `EditorSelectAll`, `TogglePalette`, `NewTerminal`,
        // `NewAgentPane`) onto a label, an optional keystroke spec, and a row position - so the
        // Windows/Linux in-window popover (`crate::title_bar::menu`) and the real macOS menu
        // (`crate::title_bar::native_menu`, a later revision) can never drift onto two different
        // command sets. No `KeyBinding` is added alongside any of these here; the one real
        // `cmd-q` binding this issue needs is added separately once `Quit` has a real handler.
        OpenFile,
        OpenFolder,
        NewWindow,
        CloseWindow,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        NextAgent,
        PreviousAgent,
        ArchiveAgent,
        ReviewAgent,
        KeepAllChanges,
        DiscardWorktree,
        OpenDocumentation,
        ReportIssue,
        About,
        // GitHub issues #302-#305: the interactive-rebase plan's own six keyboard actions, the
        // real bindings behind design spec §1.4's footer hint strip (`alt+↑↓ reorder · P pick ·
        // S squash · D drop · mod+enter start`). Registered in `crate::default_key_bindings`
        // under `"rebase-plan && !text-input"` (`"rebase-plan"` for `RebaseStart`) - see that
        // function's own docs for why the negated conjunct is load-bearing here: a plan row's
        // `reword` field is a real text input *inside* the rebase surface, so `P`/`S`/`D` and
        // `alt+↑↓` must go dead the moment it takes focus or typing "p" into a commit message
        // would silently rewrite that row's action instead.
        RebaseReorderUp,
        RebaseReorderDown,
        RebasePickRow,
        RebaseSquashRow,
        RebaseDropRow,
        RebaseStart,
        // GitHub issue #288's two diff-line review-note gestures. Both are scoped to the
        // `"diff-view"` key context the notes surface puts on its own container
        // (`crate::review_notes::render::AdeApp::wrap_diff_with_notes`), never global - see
        // `crate::default_key_bindings` for each one's own scoping, and in particular why
        // `ToggleLineNote`'s plain `c` needs the `&& !text-input` conjunct that `SendReviewNotes`
        // deliberately does not.
        SendReviewNotes,
        ToggleLineNote,
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

/// How often [`AdeApp::start_repo_worktrees_polling`]'s sweep re-fetches every *non-focused*
/// [`AdeApp::repos`] entry's own [`Repo::worktrees`] - see that method's own docs for the full
/// reasoning. 5x [`STATUS_POLL_INTERVAL`]: a repo you aren't looking at right now has its
/// worktree list (branches created/removed via `git worktree add`/`remove`) change far less
/// often than an open agent's own running/idle/failed status does, and the *focused* repo
/// already gets a near-instant refresh from its own real filesystem watcher
/// ([`AdeApp::start_worktree_watch`]) - a background repo only needs to be "eventually live
/// within a bounded window", not sub-second-fresh, so trading a little staleness there for far
/// fewer real `git` subprocess spawns (a user can plausibly have dozens of repos added) is the
/// right default.
pub(crate) const REPO_WORKTREES_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// The real cap on how many `git worktree list` subprocesses [`AdeApp::
/// start_repo_worktrees_polling`]'s sweep lets run at once, regardless of how many repos are due
/// for a refresh on a given tick - see [`crate::rail::repo::batch_repos_for_refresh`]'s own docs
/// for the batching this bounds. Small and arbitrary-but-reasonable: large enough that a sweep
/// across a realistic handful of added repos still finishes in roughly one batch, small enough
/// that a user with dozens of repos added never fires dozens of real `git` child processes at
/// once on a single tick.
pub(crate) const REPO_WORKTREES_FETCH_CONCURRENCY: usize = 4;

/// How often [`AdeApp::render_file_view`] calls `std::fs::metadata` for its freshness check -
/// throttled rather than unconditional-per-render (see
/// [`AdeApp::file_view_last_freshness_check`]).
pub(crate) const FILE_FRESHNESS_CHECK_INTERVAL: Duration = Duration::from_millis(500);

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
    /// deliberately still a flat, single-repo list rather than living on the [`Repo`] itself: the
    /// file tree, diff view, command palette, and agent-spawn machinery all stay genuinely
    /// single-repo-scoped (they only ever operate on whatever this window currently has focused),
    /// so this remains their one real source of truth. [`Repo::worktrees`] is a *second*, parallel
    /// data source - kept live for every added repo, focused or not, purely for the rail's own
    /// passive per-repo group listing (see that field's own docs and [`Self::load_worktrees`],
    /// which mirrors this exact list into the focused repo's own [`Repo::worktrees`] entry so the
    /// two never disagree) - not a replacement for this field.
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
    /// The rail's **Problems** view's own real overlay scrollbar handle (GitHub issue #30) - a
    /// plain `gpui::ScrollHandle`, genuinely few rows so no virtualization is needed. The
    /// Worktrees view no longer shares this: see [`Self::rail_list_state`].
    pub(crate) rail_scroll_handle: gpui::ScrollHandle,
    /// The rail's **Worktrees** view's own real virtualized-list state (GitHub issue #364) -
    /// `crate::rail::render::AdeApp::render_rail_list` used to build every repo header, worktree
    /// row, agent row and history row
    /// eagerly, on every render, regardless of scroll position, which is the real reason the rail
    /// became slow to hover with many worktrees/agents open (GPUI's own hover mechanism forces a
    /// full `Window::refresh()` on every hover-region transition, and a refresh bypasses every
    /// view's own per-entity render cache - see that same module's docs for why). `gpui::ListState`
    /// (not `gpui::UniformListScrollHandle`) because this list's rows genuinely differ in height -
    /// the same reason [`Self::changes_sections_list`] already uses one.
    pub(crate) rail_list_state: gpui::ListState,
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
    /// The right panel's Search tab (GitHub issue #162): its four real inputs, the modifier
    /// toggles, per-file collapse, and the last completed search - see
    /// `crate::search::state::SearchPanel`.
    pub(crate) search: crate::search::state::SearchPanel,
    /// The result tree's own real virtualized-list state (GitHub issue #162's own live-report
    /// follow-up: with many results shown, the panel used to build every file row and every match
    /// row unconditionally, on every render, exactly the eager-`.children(...)` gap the rail's own
    /// `Self::rail_list_state` was fixed for). `gpui::ListState`, not a `UniformListScrollHandle`:
    /// the tree is two-level with per-file collapse, so its rows are not uniform in count *or*
    /// height. `crate::search::render::AdeApp::render_search_body`'s own `gpui::list` builds only
    /// the rows its viewport (plus `crate::search::render::SEARCH_LIST_OVERDRAW`) actually covers.
    /// The result cap (`crate::search::engine::MAX_MATCHES`) still bounds how many rows this can
    /// ever hold in total - it just no longer means every one of them is built on every frame.
    pub(crate) search_list_state: gpui::ListState,
    /// The in-flight debounced search - superseded rather than cancelled at the `gpui::Task`
    /// level, with a generation guard on the result so a slow walk cannot overwrite a newer,
    /// faster one. The walk *itself* cooperatively cancels via [`Self::search_generation`] - see
    /// that field's own docs for why a generation guard on the result alone was not enough.
    pub(crate) _search_task: Option<Task<()>>,
    /// A real, cross-thread mirror of `self.search.generation`
    /// (`crate::search::state::SearchPanel::generation`) - GitHub issue #162's own live-report
    /// follow-up. Before this, a superseded search kept running to completion on the background
    /// executor even after a newer keystroke's own debounce fired and started a second one: the
    /// generation guard only discarded the *result*, so a fast typist against a large-enough
    /// worktree could pile up several full walks competing for CPU at once, each one slowing down
    /// the one that would actually answer the query on screen. `crate::search::render::AdeApp::
    /// start_search` bumps this alongside `self.search.generation`, and `crate::search::engine::
    /// search_worktree_cancellable` polls it (via the `is_stale` closure) once per scan batch, so
    /// a superseded walk stops within one batch of being superseded rather than running to the
    /// end. Plain `u64`, not the panel's own type, so `crate::search::engine` stays GPUI-free and
    /// thread-agnostic - this is the one `Arc<AtomicU64>` cell that crosses into it.
    pub(crate) search_generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The in-flight Replace all / per-file replace.
    pub(crate) _search_replace_task: Option<Task<()>>,
    /// The `mod+F` find bar over the focused file view, or `None` while it is closed
    /// (GitHub issue #162's own in-file-find section). An `Option` rather than an always-present
    /// struct with an `open` flag: the bar has no state worth keeping between openings, and a
    /// live `FocusHandle` that no rendered node tracks is exactly the dangling-focus hazard
    /// `crate::root::OverlayFocus` exists for.
    pub(crate) find_bar: Option<crate::search::in_file::FindBar>,
    /// The find bar's focus handle - permanent, and wired into `crate::root::caret_blink` at
    /// start-up like every other field's, even though the bar itself comes and goes. See
    /// `crate::search::in_file::FindBar::focus_handle`'s own docs for why it is not minted per
    /// opening.
    pub(crate) find_bar_focus_handle: FocusHandle,
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
    /// Every open agent's own **review** state (GitHub issue #225) - its captured baseline, the
    /// diff loaded against that baseline, and which file it has open. Keyed by
    /// [`crate::work_surface::agents::AgentId`].
    pub(crate) agent_reviews: HashMap<AgentId, review::state::AgentReview>,
    /// The on-disk mirror of [`Self::agent_reviews`]' baselines - see
    /// `crate::review::baseline_state`'s module docs, including the honest note that nothing can
    /// read these back into a live review yet.
    pub(crate) review_baseline_state: review::baseline_state::ReviewBaselineState,
    /// Where [`Self::review_baseline_state`] is persisted - a sibling of the real `settings.toml`,
    /// or `None` for a test that hasn't opted into real persistence (in which case saving is a
    /// genuine no-op, exactly like [`Self::tab_order_path`]).
    pub(crate) review_baseline_path: Option<PathBuf>,
    /// Which persisted baseline keys *this* instance owns, for the merge-not-clobber write path -
    /// mirrors [`Self::tab_order_owned`]. See
    /// `crate::review::baseline_state::ReviewBaselineState::save_merged_at` for the one way this
    /// file's merge deliberately differs from its siblings'.
    pub(crate) review_baselines_owned: std::collections::BTreeSet<String>,
    /// `Some(id)` while a real `Mark reviewed` snapshot is running for that agent - guards against
    /// a double-click starting two overlapping `git write-tree` runs against the same worktree,
    /// mirroring [`Self::worktree_history_op_in_flight`]'s own single-flight discipline.
    pub(crate) review_mark_in_flight: Option<AgentId>,
    /// The live agent-hook side-channel for this launch (GitHub issue #239 phase 2): a loopback
    /// listener plus the generated `--settings`/forwarder files every Claude agent is spawned
    /// against. `None` when hook support couldn't start (an unsupported platform, a loopback that
    /// wouldn't bind, an unwritable temp directory) *or* for the many tests that never opted into
    /// it - in both cases every agent simply falls back to the Phase 1 terminal-title and
    /// quiescence signals, which is exactly the pre-phase-2 behaviour.
    pub(crate) hook_runtime: Option<crate::hooks::HookRuntime>,
    /// Whether bring-up of [`Self::hook_runtime`] has already been attempted, so a *failed*
    /// attempt is not silently retried on every subsequent Claude spawn - see
    /// `crate::hooks::flow::AdeApp::hook_injection_for`.
    pub(crate) hook_runtime_tried: bool,
    /// The on-disk record of what [`Self::hook_runtime`] learned, for GitHub issue #227 to build
    /// on - see `crate::hooks::store`'s module docs, including the honest note that no UI reads
    /// it back yet.
    pub(crate) agent_status_state: crate::hooks::store::AgentStatusState,
    /// Where [`Self::agent_status_state`] is persisted - a sibling of the real `settings.toml`, or
    /// `None` for a test that hasn't opted into real persistence, mirroring
    /// [`Self::review_baseline_path`].
    pub(crate) agent_status_path: Option<PathBuf>,
    /// Which persisted agent-status keys *this* instance owns, for the merge-not-clobber write
    /// path - mirrors [`Self::review_baselines_owned`].
    pub(crate) agent_status_owned: std::collections::BTreeSet<String>,
    /// Who wrote each line of each file, per worktree (GitHub issue #284) - see
    /// `crate::provenance` for the model and `crate::provenance::flow` for the three wires that
    /// feed and read it.
    pub(crate) line_provenance: crate::provenance::store::ProvenanceStore,
    /// Where [`Self::line_provenance`] is persisted - a sibling of the real `settings.toml`, or
    /// `None` for a test that hasn't opted into real persistence, mirroring
    /// [`Self::agent_status_path`].
    pub(crate) line_provenance_path: Option<PathBuf>,
    /// Which persisted worktree keys *this* instance owns, for the merge-not-clobber write path -
    /// mirrors [`Self::agent_status_owned`].
    pub(crate) line_provenance_owned: std::collections::BTreeSet<String>,
    /// [`Self::diff_state`]'s file list joined with [`Self::line_provenance`] - one row per path,
    /// each carrying its authors and the per-author `split` (GitHub issue #284).
    pub(crate) change_set: crate::provenance::change_set::ChangeSet,
    /// The **Uncommitted** scope (GitHub issue #285): the working tree against its own `HEAD`,
    /// loaded by the same background task [`Self::diff_state`] is, so the panel's four sections
    /// never describe two different moments in git's history.
    pub(crate) uncommitted_diff: sections::ScopeLoad<wt_core::diff::WorktreeDiff>,
    /// [`Self::uncommitted_diff`]'s file list joined with [`Self::line_provenance`] - one row per
    /// path, each carrying its authors and the per-author `split`.
    pub(crate) uncommitted_change_set: crate::provenance::change_set::ChangeSet,
    /// Every diff-line review note this window holds (GitHub issue #288), keyed worktree -> path
    /// -> line anchor. See `crate::review_notes` for the model and the three rules it enforces.
    pub(crate) review_notes: crate::review_notes::NoteStore,
    /// Where [`Self::review_notes`] is persisted - `review-notes.toml`, next to `settings.toml`.
    /// `None` only when this instance has no real settings path at all (an in-test app).
    pub(crate) review_notes_path: Option<PathBuf>,
    /// Which encoded worktree keys this window may rewrite in that file - the same
    /// merge-ownership set [`Self::line_provenance_owned`] keeps, and for the same reason.
    pub(crate) review_notes_owned: std::collections::BTreeSet<String>,
    /// The one note currently open for typing, if any - see `crate::review_notes::flow::NoteDraft`
    /// for why there is exactly one rather than one per card.
    pub(crate) note_draft: Option<crate::review_notes::flow::NoteDraft>,
    /// The diff line `C` would toggle a note on: the last one a note was opened on. There is no
    /// diff-line caret in this read-only virtualized list, so this is the whole of "the line you
    /// are on" - see `crate::review_notes::flow::AdeApp::handle_toggle_line_note`.
    pub(crate) note_cursor: Option<crate::review_notes::NoteRef>,
    /// Why the last send failed, if it did. Shown in the notes bar rather than swallowed: a review
    /// note that silently reached nobody is the worst outcome this feature has.
    pub(crate) note_send_error: Option<crate::review_notes::flow::NoteSendError>,
    /// Focus for the open note card's own hand-rolled single-line input.
    pub(crate) note_focus_handle: FocusHandle,
    /// Focus for the diff pane's notes container, which is what makes its `"diff-view"` key
    /// context - and so the `mod+enter`/`c` bindings the bar draws as keycaps - really live.
    pub(crate) diff_notes_focus_handle: FocusHandle,
    /// The per-author diff filter (GitHub issue #287), if one is in force: which file it was
    /// entered from, and whose lines it keeps at full opacity.
    pub(crate) author_filter: Option<crate::provenance::render::AuthorFilter>,
    /// The **Commits** scope (GitHub issue #285): what is already written down on this branch.
    pub(crate) branch_commits: sections::ScopeLoad<wt_core::diff::BranchCommits>,
    /// Which of the Changes panel's four sections are open. Per section, not per worktree -
    /// "show me the commits" is a statement about how the user is working right now, not a
    /// property of one checkout, and the mock keys it the same way.
    pub(crate) changes_sections: sections::SectionCollapse,
    /// The Changes panel's one scroller (GitHub issue #285). `gpui::ListState`, not a
    /// `UniformListScrollHandle`: the four sections are one scroller holding genuinely different
    /// row heights (24px header, 27px file row, 48px two-line run row), which `uniform_list`
    /// cannot represent - it sizes every slot from item 0. See `crate::sidebar::sections`'
    /// [`sections::SectionRow`] for the item model, and `crate::root::scrollbar` for how the one
    /// shared overlay scrollbar still draws against it.
    pub(crate) changes_sections_list: gpui::ListState,
    /// Which changed files have been **seen since the agent last changed them**.
    pub(crate) seen_files: sections::SeenFiles,
    /// Real expand/collapse state for the file tree - a directory's absolute path is in this set
    /// iff it is expanded (see `crate::sidebar::file_tree::visible_entries`, which this set feeds
    /// directly). **Absence means collapsed**, so a worktree opened for the first time shows only
    /// its root-level entries (GitHub issue #18 §1).
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
    pub(crate) file_tree_complete: bool,
    /// Which view the left column's sidebar strip has selected (GitHub issue #291) - see
    /// `crate::rail::strip::SidebarView`.
    pub(crate) sidebar_view: crate::rail::strip::SidebarView,
    /// The rail's open worktree/agent row menu (GitHub issue #290), `None` when closed - see
    /// `crate::rail::menu::RailRowMenu`. Its origin is window-space and already clamped, and it
    /// is rendered from [`Render::render`] rather than from the rail, because the rail's row list
    /// is a real scroller and a menu inside it would be clipped by it and would scroll away from
    /// the pointer it was anchored to (`REVISION-2026-08-14.md` §4).
    pub(crate) rail_row_menu: Option<crate::rail::menu::RailRowMenu>,
    /// The rail's open overflow menu (GitHub issue #290), `None` when closed. A separate surface
    /// from [`Self::rail_row_menu`] because it is anchored off the button's own rect rather than
    /// the pointer, and opens from a control rather than from a row - see
    /// `crate::rail::menu::RailOverflowMenu`.
    pub(crate) rail_overflow_menu: Option<crate::rail::menu::RailOverflowMenu>,
    /// The overflow button's real, `gpui::canvas`-captured window-space rect - what §4w's "the
    /// overflow menu off the ⋯ button's own rect with right edges aligned" is measured from. The
    /// same capture `Self::commit_composer_bounds` uses, and for the same reason: the menu is a
    /// root-level sibling, so it cannot read its opener's layout any other way.
    pub(crate) rail_overflow_button_bounds: gpui::Bounds<Pixels>,
    /// Which worktree's `Remove worktree…` row has had its *first* click (GitHub issue #290),
    /// `None` when nothing is armed - the same in-menu two-click confirmation
    /// `Self::graph_state.delete_branch_confirm_armed` gives the git graph's `Delete branch` row,
    /// keyed by worktree path because that is what the removal really acts on. Cleared by every
    /// path that closes the rail row menu (`crate::root::menus::AdeApp::close_menu_surface`), so
    /// a menu dismissed by any means at all can never leave a worktree armed for a one-click
    /// removal the next time it opens.
    pub(crate) remove_worktree_confirm_armed: Option<PathBuf>,
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
    /// Every path in the currently-loaded worktree with a *live* uncommitted delta - staged,
    /// unstaged, or untracked (`wt_core::stage::dirty_paths`, a real `git status --porcelain`
    /// read), or `None` while that answer isn't known yet.
    pub(crate) dirty_files: Option<HashSet<PathBuf>>,
    /// The most recent real git failure from one Changes row's own controls - `(path, message)`.
    /// Two writers today: [`Self::toggle_staged`]'s `git add`/`git reset`, and
    /// [`Self::discard_change_row`]'s `git checkout HEAD -- <path>` (GitHub issue #286's floating
    /// hover bar). One channel rather than one per action, because it is one place on screen: the
    /// row acted, the row's action failed, and the panel says so once, immediately under the
    /// composer.
    pub(crate) changes_row_error: Option<(PathBuf, String)>,
    /// Every in-flight [`Self::toggle_staged`] background `git add`/`git reset` - a [`TaskPool`],
    /// not a single slot, for the same "independent operations" reason as
    /// [`Self::_merge_write_tasks`]: two different Changes rows' checkboxes clicked in quick
    /// succession are two genuinely independent real git operations, and a shared single slot
    /// would silently cancel (and so leave un-applied) whichever one didn't win the race for the
    /// slot.
    pub(crate) _stage_tasks: TaskPool,
    /// Which Uncommitted row's own 27px band the pointer is inside, if any - one half of what
    /// reveals `STAGE-A-CHANGELOG.md` §4i's floating hover bar.
    pub(crate) change_row_hover: Option<PathBuf>,
    /// The other half of [`Self::change_row_hover`] - the floating bar's own hitbox, including
    /// the part of it that hangs above the row. See that field's docs.
    pub(crate) change_row_actions_hover: Option<PathBuf>,
    /// The row whose `Discard` button is **armed**: its icon has been clicked once and swapped
    /// for the red `Discard?` pill, and a second click really discards.
    pub(crate) change_row_discard_armed: Option<PathBuf>,
    /// Every in-flight [`Self::discard_change_row`] background `git checkout`/`git rm` - a
    /// [`TaskPool`] for the same reason [`Self::_stage_tasks`] is one.
    pub(crate) _discard_tasks: TaskPool,
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
    /// Every `FocusHandle` [`Self::wire_caret_blink`] has subscribed - the same handles
    /// [`Self::_caret_blink_subscriptions`] covers, kept as a live list so a blur can ask "is
    /// some *other* caret-bearing surface focused right now?" rather than assuming it is the
    /// last word on the focus change it is reporting. That question is what makes
    /// [`Self::stop_caret_blink_on_blur`] independent of the order GPUI happens to run these
    /// subscriptions in - see its own docs for the real bug that ordering caused.
    pub(crate) caret_blink_handles: Vec<FocusHandle>,
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
    /// `Some(id)` when the review tab (GitHub issue #225) is open, for that agent. At most one
    /// review tab exists per window - opening a review for a different agent retargets this one
    /// rather than accumulating tabs, the same "one per window" shape [`Self::graph_tab_open`]
    /// uses - but unlike the graph tab it carries which agent it's *for*, since a review is
    /// inherently per-agent (that is the entire point of the feature).
    pub(crate) review_tab_open: Option<AgentId>,
    /// Whether the review tab is the tab strip's currently *active* entry - exactly mirrors
    /// [`Self::graph_tab_active`], including that switching to another tab clears this without
    /// closing the review tab.
    pub(crate) review_tab_active: bool,
    /// The review tab's own keyboard-focus target, `track_focus`'d by
    /// `crate::review::render::AdeApp::render_review_view`'s container - and swept exactly like
    /// [`Self::graph_focus_handle`] whenever the tab stops being rendered (see
    /// `crate::review::render::AdeApp::leave_review_tab`).
    pub(crate) review_focus_handle: FocusHandle,
    /// Pre-open focus target for [`Self::review_focus_handle`] - see [`OverlayFocus`], and
    /// [`Self::graph_focus`] for the identical role on the graph tab.
    pub(crate) review_focus: OverlayFocus,
    /// Which runs the sidebar's History view is showing - `all` or `this worktree`
    /// (`design_handoff_jerry_ade/revision 5/REVISION-2026-08-14.md` §6, GitHub issue #227).
    pub(crate) history_scope: crate::run_history::model::HistoryScope,
    /// Which History worktree groups the user has explicitly folded or unfolded, keyed by
    /// worktree path (`true` = folded). A worktree with no entry takes the default
    /// `crate::run_history::model::build_run_tree` computes - the active one opens, the rest do
    /// not - which is why this is a map of *explicit* choices rather than a set of open groups:
    /// the two are only the same until the active worktree changes.
    pub(crate) history_collapsed: HashMap<PathBuf, bool>,
    /// Real drift counts, `worktree -> run key -> commits since that run ended` (GitHub issue
    /// #227). Filled in by `crate::run_history::flow::AdeApp::load_run_drift`, which runs a real
    /// `wt_core::run_drift::commits_since_each` per worktree on the background executor.
    pub(crate) run_drift: HashMap<PathBuf, HashMap<String, usize>>,
    /// Whether a drift load is already running, so a re-render mid-flight cannot start a second
    /// one - the same single-flight shape [`Self::prune_in_flight`] uses.
    pub(crate) run_drift_in_flight: bool,
    /// Which run's transcript is open as a centre tab in each worktree, keyed by worktree path.
    pub(crate) run_tab_by_worktree: HashMap<PathBuf, String>,
    /// Whether the run-transcript tab is the tab strip's currently *active* entry - exactly
    /// mirrors [`Self::review_tab_active`], including that switching to another tab clears this
    /// without closing the run tab.
    pub(crate) run_tab_active: bool,
    /// The run-transcript tab's own keyboard-focus target, swept exactly like
    /// [`Self::review_focus_handle`] whenever the tab stops being rendered.
    pub(crate) run_tab_focus_handle: FocusHandle,
    /// Pre-open focus target for [`Self::run_tab_focus_handle`] - see [`OverlayFocus`].
    pub(crate) run_tab_focus: OverlayFocus,
    /// The run-transcript body's scroll handle - a transcript is longer than the pane.
    pub(crate) run_tab_scroll_handle: gpui::ScrollHandle,
    /// Real captured transcripts read back off disk, keyed by run id
    /// (`crate::run_history::transcript_store`). A key present with `None` means "this run was
    /// looked up and genuinely has no stored transcript", which is what makes the synthesised
    /// body (`crate::run_history::model::transcript_body`) a decision rather than a race: without
    /// the distinction, every first frame after opening a tab would render the synthesis and then
    /// swap it for the real thing.
    pub(crate) run_transcripts: HashMap<String, Option<Vec<String>>>,
    /// Where [`Self::run_transcripts`] is persisted - a sibling of `agent-status.toml`, or `None`
    /// when this instance has no real settings path (see `crate::settings::store`).
    pub(crate) run_transcript_dir: Option<PathBuf>,
    /// In-flight background reads of a run's stored transcript, keyed by run id. Keyed rather
    /// than a single slot for [`Self::_review_baseline_tasks`]'s own reason: a `Task` cancels on
    /// drop, and two tabs opened in quick succession must not cancel each other's read.
    pub(crate) _run_transcript_load_tasks: HashMap<String, Task<()>>,
    /// In-flight "this run just ended" captures, keyed by run id - the real diffstat measurement
    /// and record write `crate::run_history::flow::AdeApp::finish_run_record` performs when an
    /// agent's pane closes. Keyed for the same reason, and removed by each task as it completes.
    pub(crate) _run_finish_tasks: HashMap<String, Task<()>>,
    /// The in-flight drift traversal - one slot, guarded by [`Self::run_drift_in_flight`].
    pub(crate) _run_drift_task: Option<Task<()>>,
    /// The review tab's own overlay-scrollbar handle - its own, not
    /// [`Self::diff_view_scroll_handle`]: the two surfaces are separate places in the app, and
    /// sharing one handle would carry the git Diff view's scroll position into the review (and
    /// back) every time the user switched between them.
    pub(crate) review_scroll_handle: UniformListScrollHandle,
    /// [`Self::diff_highlight_cache`]'s counterpart for the review tab's own open file - a
    /// separate cache for the same reason the scroll handle is separate: the review tab and the
    /// git Diff view can each have a *different* file open, and one shared cache would thrash
    /// (its identity guard rejecting every read) every time the user moved between them. Kept
    /// fresh by `crate::review::render::AdeApp::refresh_review_highlight_cache`.
    pub(crate) review_highlight_cache: Option<DiffHighlightCache>,
    /// The in-flight `wt_core::review::snapshot_worktree_tree` behind each fresh agent's
    /// baseline capture, keyed by agent.
    pub(crate) _review_baseline_tasks: HashMap<AgentId, Task<()>>,
    /// The in-flight `wt_core::review::diff_against_tree` load behind the review tab.
    pub(crate) _review_load_task: Option<Task<()>>,
    /// The in-flight `Mark reviewed` re-snapshot - guarded by [`Self::review_mark_in_flight`].
    pub(crate) _review_mark_task: Option<Task<()>>,
    /// In-flight `wt_core::review::delete_ref` calls releasing closed agents' baseline refs,
    /// keyed by the agent that was closed - same reasoning as [`Self::_review_baseline_tasks`]:
    /// with one shared slot, closing two agents in quick succession cancelled the first one's
    /// deletion and leaked that ref forever.
    pub(crate) _review_release_tasks: HashMap<AgentId, Task<()>>,
    /// The in-flight background save of [`Self::review_baseline_state`] - one slot is correct
    /// here (unlike the two above), because every save writes the *whole* merged state, so a
    /// newer one genuinely supersedes an older one. Mirrors `AdeApp::_tab_order_save_task`.
    pub(crate) _review_persist_task: Option<Task<()>>,
    /// Holds the in-flight write of [`Self::agent_status_state`] - see
    /// `crate::hooks::flow::AdeApp::record_agent_statuses`. A `Task` cancels on drop, so this
    /// must be stored for the write to actually land.
    pub(crate) _agent_status_persist_task: Option<Task<()>>,
    /// Holds the in-flight write of [`Self::line_provenance`] - see
    /// `crate::provenance::flow::AdeApp::persist_line_provenance`. One slot, newest wins: the
    /// state is captured whole at spawn time, so a newer write genuinely supersedes an older one.
    pub(crate) _line_provenance_persist_task: Option<Task<()>>,
    /// Holds the in-flight *immediate* write of [`Self::review_notes`] - see
    /// `crate::review_notes::flow::AdeApp::persist_review_notes`. One slot, newest wins, exactly
    /// like the provenance write above it.
    pub(crate) _review_notes_persist_task: Option<Task<()>>,
    /// Holds the in-flight **debounced** write, kept apart from the immediate one on purpose: a
    /// `Task` cancels on drop, so one shared slot would let the next keystroke's timer cancel a
    /// write that a closing card or a send had already committed to. See
    /// `crate::review_notes::flow::AdeApp::schedule_review_notes_persist`.
    pub(crate) _review_notes_debounce_task: Option<Task<()>>,
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
    /// The read-only Diff view's own scroll handle, and the source its real overlay scrollbar
    /// (GitHub issue #30) reads its geometry from. A `gpui::UniformListScrollHandle`, the same
    /// type [`Self::file_view_scroll_handle`] uses, since GitHub issue #224 turned
    /// `crate::code_surface::diff_view::AdeApp::render_diff_file_detail`'s eager
    /// `overflow_y_scroll()` div into a real `gpui::uniform_list`, which owns its own scroll
    /// offset through this handle type rather than a plain `gpui::ScrollHandle`. The scrollbar
    /// itself needed no change: `crate::root::scrollbar::ScrollableHandle` already treats both
    /// handle kinds interchangeably.
    pub(crate) diff_view_scroll_handle: UniformListScrollHandle,
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
    /// The same idea as [`Self::file_view_row_layout`], for the app's hand-rolled single-line
    /// inputs (GitHub issue #336): every `widgets::render_simple_input_row` captures its own real
    /// painted bounds and shaped line here each frame, and its click/drag handlers read them back
    /// to hit-test a pointer x into a real byte offset (`gpui::LineLayout::closest_index_for_x`).
    /// Keyed by the row's own `widgets::SimpleInput::caret_selector` - see that field's own docs
    /// for why the caret selector rather than the text one. Transient/best-effort in exactly
    /// the same way: an entry for a field no longer on screen is simply never refreshed, and can
    /// never be read because it can't be clicked.
    pub(crate) simple_input_layout:
        HashMap<gpui::SharedString, (gpui::Bounds<Pixels>, gpui::ShapedLine)>,
    /// Which single-line input a real click-drag selection is currently in progress in - the
    /// `caret_selector` key of [`Self::simple_input_layout`], or `None` when no button is down.
    pub(crate) simple_input_drag: Option<gpui::SharedString>,
    /// GitHub issue #202: which code blocks the user has currently collapsed, keyed by absolute
    /// path, each value a set of 0-based [`code_surface::fold::FoldRange::start_line`]s.
    pub(crate) file_view_folds: HashMap<PathBuf, std::collections::HashSet<usize>>,
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
    /// Which step the palette is showing - the flat root list, or a command's own drill-down
    /// (`crate::palette::state::PaletteStep`). Reset to `Root` by
    /// [`Self::open_palette`]/[`Self::close_palette`], so a palette never reopens mid-question,
    /// and by `Esc` inside a step, which goes back rather than closing.
    pub(crate) palette_step: palette::PaletteStep,
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
    /// The Changes panel's commit message field - genuinely editable text with the same real,
    /// undoable history every other field in this app gets (GitHub issue #17 -
    /// [`text_history::TextField`]). A normal, empty text input the user has to fill in
    /// themselves - no auto-drafted fallback (removed per explicit product decision: see
    /// [`crate::sidebar::render::AdeApp::staged_commit_message`]'s own docs for why). Every commit
    /// path (the primary button, the `▾` menu) reads this same field, so they always agree on
    /// what gets written, and none of them will act at all with it empty.
    pub(crate) commit_message: text_history::TextField,
    pub(crate) commit_message_focus_handle: FocusHandle,
    /// The rail's *root container*'s focus handle - the app's real "nowhere else to put focus"
    /// fallback target (`Self::select_worktree`, `Self::close_agent`, `Self::cancel_new_file`),
    /// deliberately **not** [`Self::filter_focus_handle`].
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
    /// The real instant [`Self::process_stats`] was last written by the background poll - the
    /// Resources popover's `Updated Ns ago` line (GitHub issue #293). `None` until the very first
    /// sample lands, which that line renders as an honest `not sampled yet` rather than as
    /// "0s ago".
    pub(crate) process_stats_sampled_at: Option<std::time::Instant>,
    /// Whether the status bar's Resources popover is open (GitHub issue #293) - one of the
    /// [`menus::MenuSurface`]s, so it obeys the app's one-menu-at-a-time invariant.
    pub(crate) resources_popover_open: bool,
    /// The status bar's `X% cpu · Y GB` readout's real painted bounds, captured through the same
    /// `gpui::canvas` idiom [`Self::plus_button_bounds`] uses, so the Resources popover can be
    /// positioned off the control that opens it.
    pub(crate) resources_readout_bounds: gpui::Bounds<Pixels>,
    /// Every provider's real rate-limit budget (GitHub issue #294) - what the agent pane strip's
    /// cluster and the budget popover both read.
    pub(crate) budget: crate::budget::state::BudgetState,
    /// Whether the agent pane's rate-limit budget popover is open (GitHub issue #294) - one of
    /// the [`menus::MenuSurface`]s, so it obeys the app's one-menu-at-a-time invariant.
    pub(crate) budget_popover_open: bool,
    /// The pane strip's budget cluster's real painted bounds, captured through the same
    /// `gpui::canvas` idiom [`Self::resources_readout_bounds`] uses, so the popover opens
    /// directly above the control that opened it (§4u′).
    pub(crate) budget_readout_bounds: gpui::Bounds<Pixels>,
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
    /// The git graph's `Merge into current branch\u{2026}` action and Surface D's
    /// conflict-resolution flow - see [`crate::merge::state::MergeFlow`]'s docs. `None` when no
    /// agent has an in-flight merge or unresolved conflict. (The agent context bar's own `Merge`
    /// button was deleted by GitHub issue #295; see
    /// `crate::merge::flow::AdeApp::start_merge`'s docs for the direction that is waiting on
    /// issue #241.)
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
    /// Every in-flight one-shot [`Self::load_repo_worktrees`] fetch - a [`TaskPool`] since these
    /// are genuinely independent, fire-and-forget per-repo loads (one per newly [`Self::
    /// add_repo`]-ed repo, plus one per repo restored from `repos.toml` at startup), not a single
    /// slot the way [`Self::_load_worktrees_task`] is for the one focused repo.
    pub(crate) _repo_worktrees_tasks: TaskPool,
    /// The single, long-lived periodic sweep (`crate::root::AdeApp::
    /// start_repo_worktrees_polling`) that keeps every *non-focused* [`Self::repos`] entry's own
    /// [`Repo::worktrees`] live - started once, at startup, and never reassigned per repo switch
    /// (unlike [`Self::_worktree_watch_task`]/[`Self::_status_poll_task`], which are genuinely
    /// scoped to whichever single repo is focused): this loop reads [`Self::repos`]/[`Self::
    /// focused_repo`] fresh on every tick, so one instance already serves however many repos are
    /// added, the same "started lazily, then never reset" shape [`Self::_lsp_poll_task`] uses for
    /// an analogous "one loop, many independent things it watches" role.
    pub(crate) _repo_worktrees_poll_task: Option<Task<()>>,
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
    /// The per-provider rate-limit budget poll loop (GitHub issue #294,
    /// `crate::budget::flow::AdeApp::start_budget_poll_loop`) - kept alive here for the same
    /// reason [`Self::_update_check_task`] is, and reserved to the loop alone: an individual
    /// provider read detaches its own task rather than reassigning this field, so a manual
    /// `Refresh` can never cancel the loop out from under itself.
    pub(crate) _budget_poll_task: Option<Task<()>>,
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
    /// GitHub issue #204: the exact text the Diagnostic card's own `copy` button most recently
    /// put on the real system clipboard, for as long as that card should still be showing its
    /// momentary `copied` confirmation (see
    /// [`crate::code_surface::lsp_ui::DIAGNOSTIC_COPY_CONFIRM_DURATION`]).
    pub(crate) diagnostic_copy_confirmed: Option<String>,
    /// The single in-flight timer that clears [`Self::diagnostic_copy_confirmed`], a single slot
    /// for the same reason [`Self::_hover_hide_task`] is one: assigning a fresh task drops
    /// (cancels) the previous one, so repeatedly clicking `copy` leaves exactly one armed timer
    /// and each click genuinely restarts the confirmation rather than inheriting the first
    /// click's remaining time.
    pub(crate) _diagnostic_copy_confirm_task: Option<Task<()>>,
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
    /// [`Self::open_file_at_line`] whenever a navigation names a line in a file that isn't
    /// already open - a go-to-definition result, a terminal `path:line` link, or a sidebar
    /// Problems row. Keyed by the target path (not just a line number) so an unrelated file's
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
    /// GitHub issue #401: the Editor > Search page's "add a pattern" row - a real, focusable
    /// text input (same minimal append/backspace/`Esc`-clears/`Enter`-submits shape as
    /// [`Self::new_file_input`]'s own name field, and the same real per-widget undo history,
    /// GitHub issue #17) that appends a real, persisted entry to
    /// [`settings_store::EditorSettings::search_excludes`] on `Enter`, then clears itself for the
    /// next pattern - see `crate::settings::render::AdeApp::add_search_exclude_pattern`.
    pub(crate) search_exclude_input: text_history::TextField,
    pub(crate) search_exclude_input_focus_handle: FocusHandle,
    /// GitHub issue #141: the Themes page's "Generate from colour" seed - a real, focusable hex
    /// input (`#rrggbb`), same minimal append/backspace/`Esc`-clears shape as
    /// [`Self::settings_keymap_filter`] and the same real per-widget undo history (GitHub issue
    /// #17). Its value is what `crate::theme::shift_from_seed` derives a whole theme from.
    pub(crate) theme_seed_input: text_history::TextField,
    pub(crate) theme_seed_focus_handle: FocusHandle,
    /// GitHub issue #213: the General page's "Shell" field - the same minimal focusable
    /// text-input shape as [`Self::theme_seed_input`] (real `FocusHandle`, real caret,
    /// append/backspace/`Esc`-clears, real per-widget undo history). Seeded at startup from the
    /// persisted `settings.terminal.shell` and written straight back to it on every edit, so the
    /// field and the file never disagree.
    pub(crate) shell_input: text_history::TextField,
    pub(crate) shell_focus_handle: FocusHandle,
    /// The advisory found/not-found state of whatever [`Self::shell_input`] currently holds
    /// (`crate::settings::state::detect_shell_status`) - recomputed on each edit and when
    /// Settings opens, never inside `render` (it does real filesystem work). Advisory only: a
    /// `NotFound` never stops the app from trying to spawn the configured program - see
    /// `crate::terminal::pane::configured_shell_program`'s docs.
    pub(crate) shell_status: settings::ShellStatus,
    /// Every shell this machine genuinely has, offered as clickable suggestions under the Shell
    /// field (GitHub issue #213's follow-up - "a select + auto-detect installed shells", built as
    /// a hybrid so the field itself stays unrestricted free text).
    pub(crate) shell_suggestions: Vec<settings::ShellSuggestion>,
    /// Whether that suggestion dropdown is currently painted. A real
    /// [`crate::root::menus::MenuSurface`] like every other floating dropdown in the app, so the
    /// one-at-a-time/close-on-window-deactivation invariant covers it too and it cannot get stuck
    /// open behind another surface.
    pub(crate) shell_suggestions_open: bool,
    /// The Shell field's real painted, window-space bounds, captured through the same
    /// `gpui::canvas` idiom [`Self::plus_button_bounds`] uses, so the dropdown can be positioned
    /// directly under the field while still being a top-level sibling in [`Self::render`] - a
    /// child of the settings row itself would be clipped by the settings page's own scroll
    /// column. `Bounds::default()` until the field has really painted once.
    pub(crate) shell_field_bounds: gpui::Bounds<Pixels>,
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
    /// [`Self::combined_tab_order`]'s own docs). Two writers: [`Self::reorder_tab`] (a real drag)
    /// and `crate::work_surface::session::AdeApp::restore_worktree_session` (a worktree's
    /// remembered order, seeded at the moment it is genuinely activated). A worktree with no entry
    /// here reconciles against an empty slice, i.e. the plain "every agent, then every file"
    /// default.
    pub(crate) tab_order: HashMap<PathBuf, Vec<work_surface::TabRef>>,
    /// The tab strip's real, on-disk session (GitHub issue #16: "the resulting layout... persists
    /// per session/worktree and restores on relaunch", widened into "which tabs were open at all"
    /// by `crate::work_surface::session`) - loaded once at startup (`Self::new_with_settings`),
    /// written by `crate::work_surface::session::AdeApp::record_worktree_session` on every real tab
    /// change, and read back by that module's own restore to reopen them.
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
    /// Every worktree whose persisted tab session this window has already dealt with - see
    /// `crate::work_surface::session::AdeApp::restore_worktree_session`, which is the only writer.
    pub(crate) session_restored: HashSet<PathBuf>,
    /// Every real reason a persisted tab could not be reopened this session - "`src/gone.rs` no
    /// longer exists", "a Codex agent has no resumable session id". One degraded tab never fails
    /// the rest of a restore (see
    /// [`crate::work_surface::session::AdeApp::restore_worktree_session`]), but it must not
    /// silently vanish either: each entry is logged as it happens and kept here.
    pub(crate) session_restore_notices: Vec<String>,
    /// Which worktree of each repo (keyed by `crate::rail::repo::repo_key`) was last genuinely
    /// selected - the live mirror of `crate::rail::repo::RepoRecord::selected_worktree`, seeded
    /// from `repos.toml` at startup and updated by [`Self::select_worktree`].
    pub(crate) selected_worktree_by_repo: HashMap<String, PathBuf>,
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
    /// The next fresh id [`Self::drop_dragged_tab`] will stamp into [`Self::dropped_tab_settle`]
    /// (and, since Revision task #65, into [`Self::tab_slide`] too - one drop kicks off both
    /// animations at once, so both can safely share one fresh id per drop) - see that field's own
    /// docs for why a fresh id matters every time.
    pub(crate) next_tab_settle_id: u64,
    /// Each currently-rendered tab's own real painted bounds, captured every render by a
    /// `gpui::canvas` overlay in [`work_surface::render::AdeApp::render_tab_chrome`] - the same
    /// idiom [`Self::plus_button_bounds`] already uses, keyed by
    /// [`work_surface::TabRef`] since every tab paints its own each frame. This is the *only*
    /// real source of a tab's on-screen width (GPUI's flex layout means no two tabs are the same
    /// size), which [`Self::drop_dragged_tab`] needs to compute how far a drop's neighbouring
    /// tabs must visually slide (see [`Self::tab_slide`]'s own docs). Never pruned when a tab
    /// closes - a harmless, bounded leak, since a `TabRef` is small and the set of tabs a session
    /// ever opens is bounded by real user action.
    pub(crate) tab_bounds: std::collections::HashMap<work_surface::TabRef, gpui::Bounds<Pixels>>,
    /// The unified tab strip's real neighbour-slide animation - the "every tab other than the
    /// one actually dropped just teleports to its new slot" gap GitHub issue #16 left open
    /// (tracked internally as task #65), now closed the same way [`Self::dropped_tab_settle`]
    /// closed the dropped tab's own "instant, no visual feedback" gap. `tab_ref -> (start_offset,
    /// id)` for every tab whose horizontal slot moved as a side effect of the most recent drop -
    /// never the dragged tab itself, which keeps its own settle-fade instead
    /// (`work_surface::state::tab_slide_offsets`'s own docs on why the two are always disjoint).
    /// `start_offset` is a real pixel distance (`work_surface::state::tab_slide_offsets`'s own
    /// docs on why only the *dragged* tab's own last-measured [`Self::tab_bounds`] width is ever
    /// needed to compute it, not each shifted tab's), `id` a fresh [`Self::next_tab_settle_id`]
    /// value for the same "GPUI keys animation progress purely by id string" reason
    /// [`Self::dropped_tab_settle`]'s own docs give - doubly so here, since *multiple* sibling
    /// tabs can carry a slide at once, so [`work_surface::render::AdeApp::render_tab_chrome`]
    /// mixes each tab's own [`gpui::ElementId`] into the animation id too. Replaced wholesale by
    /// every new drop rather than merged - a stale entry left over from a finished animation is
    /// harmless (it just keeps resolving to a `0` offset, [`Self::dropped_tab_settle`]'s own
    /// "left set, never explicitly cleared" precedent), and a fresh drop's own set of shifted
    /// tabs is never a superset of the previous drop's anyway.
    pub(crate) tab_slide: std::collections::HashMap<work_surface::TabRef, (Pixels, u64)>,
    /// GitHub issue #354: the unified tab strip's own scroll state, so once the real tab row
    /// (agent/file/graph/review/run tabs, `work_surface::render::AdeApp::render_tab_strip`)
    /// overflows the strip's available width, every tab past the visible edge stays real,
    /// reachable content - scroll-wheel and drag-to-scroll through
    /// `crate::root::scrollbar::ScrollableHandle`, the same live `gpui::ScrollHandle` idiom
    /// every other scrollable region in this app already uses (`crate::root::scrollbar`'s own
    /// module docs) - never silently clipped/unreachable past the strip's own edge, which is
    /// exactly what shipped with no `.overflow_x_scroll()`/`.track_scroll()` at all before this
    /// fix. Deliberately scoped to just the tab row itself (not the `+` button, the trailing
    /// spacer, or the right-aligned agent-jump keycap cluster) - see
    /// `work_surface::render::AdeApp::render_tab_strip`'s own docs for why only that inner
    /// wrapper is the real scroll region.
    pub(crate) tab_strip_scroll_handle: gpui::ScrollHandle,
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
    /// The sound library (GitHub issue #226): built-in sounds plus whatever the user has
    /// imported into `~/.config/jerry/sounds/` at construction time - same "load once at
    /// startup, before anything needs to resolve a settings-stored id against it" seam
    /// [`Self::custom_themes`] already uses for themes. Every `Notifications` settings-page
    /// dropdown lists this; [`crate::sound::flow`]'s gating resolves a `settings.toml`
    /// `sound.*.sound` id against it before ever calling [`Self::sound_player`].
    pub(crate) sound_library: Vec<crate::sound::LibrarySound>,
    /// Real, honestly-reported load failures from the last time [`Self::sound_library`]'s user
    /// half was (re)loaded - same shape as [`Self::custom_theme_load_errors`].
    pub(crate) sound_load_errors: Vec<String>,
    /// The Notifications page's most recent import action result - same shape as
    /// [`Self::custom_theme_status`].
    pub(crate) sound_import_status: Option<Result<String, String>>,
    /// The in-flight "Import sound…" real native file-picker task
    /// (`Self::start_import_sound`) - a single slot, matching
    /// [`Self::_custom_theme_import_task`]'s own one-dialog-at-a-time reasoning.
    pub(crate) _sound_import_task: Option<Task<()>>,
    /// Which dropdown (if any) a sound-event row's "choose a sound" popover is currently open
    /// for - `None` means every one of them is closed. Mirrors
    /// `crate::settings::render::AdeApp::shell_suggestions_open`'s single-popover-at-a-time
    /// shape, keyed by event since there are three independent dropdowns on this page rather
    /// than one.
    pub(crate) sound_picker_open: Option<crate::sound::SoundEventKind>,
    /// Each sound-event row's own trigger button's real, window-space painted bounds - the same
    /// `gpui::canvas` idiom [`Self::shell_field_bounds`] uses, kept per-event (a `HashMap` rather
    /// than a single field) because all three rows exist on screen at once and the popover must
    /// anchor to whichever one was actually clicked, not whichever rendered last.
    pub(crate) sound_event_button_bounds:
        std::collections::HashMap<crate::sound::SoundEventKind, gpui::Bounds<Pixels>>,
    /// Every live agent's [`crate::sound::flow::AgentSoundState`] as of the *previous*
    /// status-poll tick, [`crate::sound::flow::AdeApp::play_agent_status_sounds`]'s own real
    /// transition memory. An agent id present here but no longer in
    /// [`crate::work_surface::agents::Agents`] (closed since the last tick) is simply never
    /// looked at again, never explicitly pruned - the same "a stale entry is harmless, not
    /// actively cleaned up" precedent [`Self::tab_slide`]'s own docs describe.
    pub(crate) prev_agent_sound_states: std::collections::HashMap<
        crate::work_surface::agents::AgentId,
        crate::sound::flow::AgentSoundState,
    >,
    /// Whether [`Self::prev_agent_sound_states`] has been populated at least once yet -
    /// `play_agent_status_sounds`'s own "don't treat a fresh app launch's already-open agents as
    /// a burst of transitions" guard. `false` until the very first status-poll tick after
    /// construction runs; every tick after that leaves it `true`, permanently, for the life of
    /// the window - see that method's own docs.
    pub(crate) agent_sound_seeded: bool,
    /// The real time [`Self::sound_player`] was last asked to play a sound *for an agent
    /// transition* (never touched by an explicit settings-page preview click) -
    /// [`crate::sound::flow::SOUND_COOLDOWN`]'s own enforcement point.
    pub(crate) last_sound_at: Option<std::time::Instant>,
    /// Whether this window currently has real OS focus - kept in sync by
    /// [`Self::_window_activation_subscription`]'s callback (GitHub issue #176's existing
    /// subscription, extended rather than duplicated) and read by
    /// [`crate::sound::flow::AdeApp::play_agent_status_sounds`]: a sound only ever plays while
    /// this is `false`, since a focused window already shows the same information visually.
    /// Starts `true` - a window is real and focused the instant its constructor finishes.
    pub(crate) window_active: bool,
    /// The real audio-thread handle every sound in this window plays through
    /// (`crate::sound::player::SoundPlayer`) - one per window, lazily opening a real output
    /// device on the first sound actually played, never before.
    pub(crate) sound_player: crate::sound::player::SoundPlayer,
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
    pub(crate) fn highlight_options(&self) -> crate::code_surface::code_view::HighlightOptions {
        crate::code_surface::code_view::HighlightOptions {
            bracket_pair_colorization: self.settings.appearance.bracket_pair_colorization,
        }
    }

    /// Drops every cached syntax-highlighting result and re-derives it, so a settings change that
    /// alters *span production* rather than paint-time colour really takes effect on already-open
    /// content instead of only on the next file opened.
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
    pub(crate) fn add_repo(&mut self, path: PathBuf, cx: &mut Context<Self>) -> RepoId {
        // The single point a repo path enters [`Self::repos`], and so the single place it gets
        // normalized - see [`repo::canonical_repo_path`]'s own docs for the real, reproduced bug
        // an unresolved path here causes (an agent spawned into the repo root vanishing from the
        // rail entirely, because its `cwd` never equals git's own answer for the same directory).
        // Callers that also *use* the path they passed for real work of their own
        // ([`Self::open_repo_in_current_window`], startup) normalize it themselves before calling
        // here rather than relying on this; both are the same idempotent call.
        let path = repo::canonical_repo_path(&path);
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
        // A real, immediate first fetch for this repo's own worktree list (see
        // `Self::load_repo_worktrees`'s own docs) - so a freshly added repo's rail group shows a
        // real count promptly rather than waiting for `Self::start_repo_worktrees_polling`'s own
        // next tick (up to `REPO_WORKTREES_POLL_INTERVAL` later). Harmless even when `id` is
        // about to be focused by the caller (`Self::open_repo_in_current_window`/startup, which
        // both call `Self::load_worktrees` right afterward): that real fetch simply mirrors the
        // same, now-current data into this same repo entry a moment later.
        self.load_repo_worktrees(id, cx);
        cx.notify();
        id
    }

    /// Makes `id` [`Self::focused_repo`] - a no-op if `id` isn't (or is no longer) in
    /// [`Self::repos`], so a stale id from a closed-over click handler can never point focus at
    /// nothing. Deliberately synchronous and cheap: unlike [`Self::select_worktree`], changing
    /// which *repo* is focused doesn't yet reload anything (the file tree/diff/agents are still
    /// single-repo-scoped fields this phase doesn't rewire - see [`Self::worktrees`]'s own docs),
    /// so there is nothing to kick off here beyond the assignment itself.
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
    pub(crate) fn open_repo_in_current_window(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Normalized *before* anything else uses it: `path` is not only stored on the `Repo`
        // below, it is also this method's own spawn cwd, `Agents::activate_for_worktree` key and
        // `Self::reset_repo_scoped_state` root. See `repo::canonical_repo_path`'s own docs.
        let path = repo::canonical_repo_path(&path);
        let id = self.add_repo(path.clone(), cx);
        self.focus_repo(id, cx);

        self.settings_focus
            .forget_target(&self.empty_state_focus_handle);
        self.palette_focus
            .forget_target(&self.empty_state_focus_handle);

        // Nothing is selected *yet* - `load_worktrees_for_opened_repo` below resolves the repo's
        // real worktree list and genuinely selects the right worktree of it, which is what
        // `Self::selected` ends up as. Cleared here (rather than left holding an index into the
        // repo being *left*) so the interim frames between this call and that fetch landing
        // render an honestly empty tab strip rather than a stale one - with
        // `Self::current_worktree_path`'s repo-root fallback gone, "nothing selected" is now a real,
        // self-consistent state everywhere instead of one that silently resolves to the repo root.
        self.selected = None;
        self.worktree_selection_notice = None;
        self.reset_repo_scoped_state(path.clone(), window, cx);
        // Owns the whole "land this repo on a real worktree and give it its guaranteed initial
        // shell there" sequence - see its own docs. This method used to spawn that shell inline,
        // into the bare `path`, and leave `Self::selected` at `None`: the reported bug, since the
        // resulting tab belonged to no worktree at all and only rendered because
        // `Self::current_worktree_path` fell back to this same repo path while nothing was selected.
        self.load_worktrees_for_opened_repo(path, cx);
        self.start_worktree_watch(cx);
        self.start_status_polling(cx);
    }

    /// The rail's real repo-switch engine - the rail-native sibling of
    /// [`Self::open_repo_in_current_window`]. It began as GitHub issue #113's "click a repo
    /// header in the rail and it checks out", but the repo header is deliberately **not
    /// clickable at all anymore** (explicit user direction, after two subtler header-click
    /// behaviors were both rejected in review - see
    /// [`crate::rail::render::AdeApp::render_repo_group`]'s own docs: in the rail, only worktree
    /// rows and agent rows are click targets). So today this has exactly one real caller:
    /// [`Self::select_worktree_by_path`]'s cross-repo fallback, reached by clicking a worktree
    /// row under a non-focused repo's group.
    pub(crate) fn checkout_repo_from_rail(
        &mut self,
        id: RepoId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.focused_repo().map(|repo| repo.id) == Some(id) {
            return;
        }
        let Some(repo) = self.repos.iter().find(|repo| repo.id == id) else {
            return;
        };
        let path = repo.path.clone();
        // Seeded synchronously from this repo's own already-known worktree list, the same
        // already-fetched data `Self::select_worktree_by_path`'s cross-repo case seeds from (see
        // its own docs for the identical reasoning) - kept fresh in the background by
        // `Self::start_repo_worktrees_polling` regardless of which repo is focused, so this is
        // usually populated by the time a real click lands here. `None` only for a repo that has
        // never had its own worktree list fetched at all (just added, or `Self::load_worktrees`'s
        // own upcoming fetch below is this repo's very first).
        let seeded_worktrees = repo.worktrees_loaded.then(|| repo.worktrees.clone());

        self.focus_repo(id, cx);

        self.settings_focus
            .forget_target(&self.empty_state_focus_handle);
        self.palette_focus
            .forget_target(&self.empty_state_focus_handle);

        // See this method's own docs: every agent open before this call, in every repo, stays
        // alive - none of them are closed here, and every one of them still shows real live
        // status in the rail (`crate::rail::render::AdeApp::build_agent_rows` folds in every
        // repo's agents, not just the focused one's). What changes here is only which repo's
        // worktree rows the rail shows - explicitly, deliberately, *never* which tab (if any) the
        // centre pane shows: focusing a repo is a pure navigation gesture, not a worktree
        // selection, so nothing may spawn from it and nothing may reactivate through it. Two real
        // gaps closed to make that hold, not one: `Self::selected` staying `None` (below) closes
        // "checking out a repo can spawn a tab attributed to the repo itself" (the reported bug,
        // back when the repo header was still clickable); `Agents::clear_active` closes the
        // other half - reactivating whichever agent was last active in `id`'s repo *looked* like
        // reasonable cross-repo persistence, but from the user's side it was the exact same "the
        // repo itself has a tab" behavior, just reached through an existing agent instead of a
        // freshly spawned one. See `Agents::clear_active`'s own docs for why this can't simply
        // be *skipped* instead - the centre pane has no repo-scoping of its own, so doing
        // nothing here would leave whatever was left's terminal rendering right alongside `id`'s
        // own, unrelated rail rows.
        self.agents.clear_active(cx);
        self.selected = None;
        self.worktree_selection_notice = None;
        // Still seeded synchronously from this repo's own already-known worktree list (see the
        // field's own docs above) - this is purely a display fix (the rail must show repo B's
        // real rows the instant repo B is focused, not repo A's stale ones for one frame), and
        // carries no selection with it.
        if let Some(items) = seeded_worktrees {
            self.worktrees = items;
        }
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
                // Read straight back out of the live mirror rather than from `Self::selected`,
                // which only ever describes the *focused* repo - see
                // [`Self::selected_worktree_by_repo`]'s own docs for why blanking every other
                // repo's remembered worktree on each save would be a real regression, not a
                // cosmetic one.
                let selected_worktree = self
                    .selected_worktree_by_repo
                    .get(&key)
                    .map(|path| path.to_string_lossy().into_owned());
                state.repos.insert(
                    key,
                    repo::RepoRecord {
                        name: r.name.clone(),
                        selected_worktree,
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
            // GitHub issue #235: wrapped in `.when(...)` (rather than the unconditional
            // registration every other `on_action` on this element uses) so
            // `gpui::App::is_action_available` genuinely reports `false` for `NewTerminal`/
            // `NewAgentPane` while `menu_command_enabled` says there's no focused repo to spawn
            // into - the real macOS menu (`crate::title_bar::native_menu`) greys its own "New
            // Terminal"/"New Agent Pane" rows off exactly that signal, with no separate
            // `disabled:` bookkeeping of its own. `new_agent`/`new_agent_pane` already no-op
            // internally on the same condition (see their own docs), so this changes what
            // `is_action_available` reports, not what a keystroke or click actually does.
            .when(self.menu_command_enabled(MenuCommand::NewTerminal), |el| {
                el.on_action(cx.listener(Self::handle_new_terminal_action))
            })
            .when(self.menu_command_enabled(MenuCommand::NewAgentPane), |el| {
                el.on_action(cx.listener(Self::handle_new_agent_pane_action))
            })
            .on_action(cx.listener(Self::handle_new_git_graph_action))
            .on_action(cx.listener(Self::handle_search_in_worktree_action))
            .on_action(cx.listener(Self::handle_next_changed_file_action))
            .on_action(cx.listener(Self::handle_toggle_change_seen_action))
            .on_action(cx.listener(Self::handle_toggle_change_staged_action))
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
            // GitHub issue #235: the `MenuCommand` variants with no existing handler anywhere
            // else in the tree - see `crate::root::menu_commands`'s own "Not every command gets
            // a `handle_*_menu_command` here" docs for the ones deliberately absent from this
            // list (they keep whatever pre-existing registration already covers their reused
            // action). Unconditional for a command `menu_command_enabled` always reports `true`
            // for; `.when(...)` for one that can genuinely be disabled, so
            // `gpui::App::is_action_available` reflects real state for the native macOS menu.
            .on_action(cx.listener(Self::handle_open_file_menu_command))
            .on_action(cx.listener(Self::handle_open_folder_menu_command))
            .on_action(cx.listener(Self::handle_new_window_menu_command))
            .on_action(cx.listener(Self::handle_close_window_menu_command))
            .on_action(cx.listener(Self::handle_open_documentation_menu_command))
            .on_action(cx.listener(Self::handle_report_issue_menu_command))
            .on_action(cx.listener(Self::handle_about_menu_command))
            .when(self.menu_command_enabled(MenuCommand::ZoomIn), |el| {
                el.on_action(cx.listener(Self::handle_zoom_in_menu_command))
            })
            .when(self.menu_command_enabled(MenuCommand::ZoomOut), |el| {
                el.on_action(cx.listener(Self::handle_zoom_out_menu_command))
            })
            .when(self.menu_command_enabled(MenuCommand::ResetZoom), |el| {
                el.on_action(cx.listener(Self::handle_reset_zoom_menu_command))
            })
            .when(self.menu_command_enabled(MenuCommand::NextAgent), |el| {
                el.on_action(cx.listener(Self::handle_next_agent_menu_command))
            })
            .when(
                self.menu_command_enabled(MenuCommand::PreviousAgent),
                |el| el.on_action(cx.listener(Self::handle_previous_agent_menu_command)),
            )
            .when(self.menu_command_enabled(MenuCommand::ArchiveAgent), |el| {
                el.on_action(cx.listener(Self::handle_archive_agent_menu_command))
            })
            .when(self.menu_command_enabled(MenuCommand::ReviewAgent), |el| {
                el.on_action(cx.listener(Self::handle_review_agent_menu_command))
            })
            .when(
                self.menu_command_enabled(MenuCommand::KeepAllChanges),
                |el| el.on_action(cx.listener(Self::handle_keep_all_changes_menu_command)),
            )
            .when(
                self.menu_command_enabled(MenuCommand::DiscardWorktree),
                |el| el.on_action(cx.listener(Self::handle_discard_worktree_menu_command)),
            )
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
            // The status bar's Resources popover (GitHub issue #293) - a window-positioned
            // overlay for exactly the reason `render_plus_menu` is one: it is placed off the
            // readout's `gpui::canvas`-captured window-space bounds, so `.absolute()` positioning
            // built from them is only correct as a direct child of this root element.
            //
            // Unlike the graph menus below it, this is *not* gated on `!self.settings_open`: the
            // status bar is an unconditional sibling of the Settings swap and keeps rendering its
            // own readout while Settings covers the workspace, so the popover that readout opens
            // must stay reachable there too.
            .when(self.resources_popover_open, |el| {
                el.child(self.render_resources_popover(cx))
            })
            // The agent pane strip's rate-limit budget popover (GitHub issue #294) - a
            // window-positioned overlay for exactly the same reason the Resources one above it
            // is: it is placed off its readout's `gpui::canvas`-captured *window-space* bounds,
            // so `.absolute()` positioning built from them is only correct as a direct child of
            // this root element. Nesting it inside the pane strip would additionally clip it to
            // an 18px-high row.
            .when(self.budget_popover_open, |el| {
                el.child(self.render_budget_popover(window, cx))
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
            // The Branches panel's own branch right-click menu (GitHub issue #241) - a
            // window-positioned overlay for exactly the reasons the two above are, plus one of its
            // own: it is anchored off a row inside the right sidebar's own scrolling panel, so
            // rendering it as a child of that panel would clip it to the panel's bounds.
            .when(
                self.graph_tab_active
                    && !self.settings_open
                    && self.graph_state.branch_menu_open.is_some(),
                |el| el.child(self.render_graph_branch_menu(cx)),
            )
            // The row menu's "Create branch here" prompt (GitHub issue #241) - a focus-owning
            // modal overlay, not a click-away menu (`crate::graph_view::state::
            // GraphBranchPrompt`'s own docs), so it lives beside the "New file" prompt
            // below rather than inside `crate::root::menus::MenuSurface` - mirrors that enum's
            // own doc comment on why "New file" is excluded from it too.
            .when(
                self.graph_tab_active
                    && !self.settings_open
                    && self.graph_state.branch_prompt.is_some(),
                |el| el.child(self.render_graph_branch_prompt(cx)),
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
                |el| el.child(self.render_commit_menu(cx)),
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
            // The rail's row menus and its `⋯` overflow (GitHub issue #290) - root-level
            // siblings, never children of the rail. `REVISION-2026-08-14.md` §4, verbatim: "All
            // menus render outside the scrolling list. Inside it they are clipped by the scroller
            // and scroll away from their anchor." The rail list is a real `overflow_y_scroll`
            // container, so both of those would happen; §4w's generalisation ("an overlay
            // anchored in viewport coordinates must live at the root. If it is nested in a panel,
            // every property of that panel - its scroll, its clip, its mount condition - becomes
            // a bug in the overlay") is why they sit here beside every other popover instead.
            //
            // Gated on `!settings_open` for the same real reason the file tree's menu below is:
            // Settings *replaces* the workspace body one child up, so an ungated menu would paint
            // a full-window occluding scrim over Settings, swallowing every click on the page
            // underneath.
            .when(!self.settings_open && self.rail_row_menu.is_some(), |el| {
                el.child(self.render_rail_row_menu(cx))
            })
            .when(
                !self.settings_open && self.rail_overflow_menu.is_some(),
                |el| el.child(self.render_rail_overflow_menu(cx)),
            )
            // Settings › General's Shell field suggestion dropdown (GitHub issue #213's
            // follow-up) - a window-positioned overlay for the same reason the `+` menu is one,
            // and more sharply so: the settings page is a scrolling column that clips its own
            // children, so this popover can only paint in full as a top-level sibling positioned
            // off `Self::shell_field_bounds`. Gated on Settings really being open on the page that
            // paints the field, so a stale anchor from an earlier frame can never float a dropdown
            // over the workspace.
            .when(
                self.settings_open
                    && self.settings_page == settings::SettingsPage::General
                    && self.shell_suggestions_open,
                |el| el.child(self.render_shell_suggestions(cx)),
            )
            // Settings › Notifications' per-event sound picker (GitHub issue #226) - same
            // "settings page clips its own children" reasoning as the Shell suggestion dropdown
            // just above, gated the same way: only while Settings is really open on the page that
            // paints these rows.
            .when(
                self.settings_open
                    && self.settings_page == settings::SettingsPage::Notifications
                    && self.sound_picker_open.is_some(),
                |el| el.child(self.render_sound_picker(cx)),
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
                    // GitHub issue #242 phase B: the interactive-rebase plan row's own drag
                    // handle needs the identical defensive cleanup, for the identical reason -
                    // `Self::cancel_rebase_row_drag` already calls `cx.notify()` itself.
                    this.cancel_rebase_row_drag(cx);
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
    pub(crate) fn forget_target(&mut self, handle: &FocusHandle) {
        if self.return_focus.as_ref() == Some(handle) {
            self.return_focus = None;
        }
    }
}

/// The shared focus-restore-on-close step for every overlay that captured a pre-open target via
/// [`OverlayFocus::capture`] - see this type's own docs for the invariant this closes.
pub(crate) fn restore_focus(
    agents: &Agents,
    overlay_focus: &mut OverlayFocus,
    fallback: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let agent_changed = agents.active_id() != overlay_focus.opened_agent;
    let restore_target = if agent_changed {
        None
    } else {
        overlay_focus.return_focus.take()
    };
    let focus_target = restore_target
        .or_else(|| agents.active().map(|agent| agent.pane.focus_handle(cx)))
        .unwrap_or(fallback);
    window.focus(&focus_target, cx);
    overlay_focus.clear();
}

/// Regression coverage for the settings-save ordering race described on
/// [`AdeApp::_settings_save_task`]'s docs: two independent per-edit tasks sharing one
/// superseding `Option<Task<()>>` slot could let an older edit's `std::fs::write` complete
/// *after* a newer edit's, since dropping a `Task` cannot stop a write that already started.
/// [`AdeApp::persist_settings`]'s serial writer loop closes this structurally.
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

    /// A real git repository with a real commit - a plain `tempfile::tempdir()` (what most tests
    /// in this module use) has no worktrees `wt_core::list_worktrees_porcelain` can report at
    /// all, which is fine for tests about agent persistence but not for one that needs a real
    /// main-worktree row to select.
    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

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

    #[gpui::test]
    fn opening_against_one_repo_does_not_erase_another_instances_already_persisted_repo(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");
        let settings_dir = tempfile::tempdir().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        let repo_state_path = repo::repo_state_path_for(&settings_path);
        let mut seed = RepoState::default();
        let canonical_b = repo::repo_key(repo_b.path()).expect("repo b key");
        seed.repos.insert(
            canonical_b.clone(),
            crate::rail::repo::RepoRecord {
                name: "repo-b".to_string(),
                selected_worktree: None,
            },
        );
        seed.save_at(&repo_state_path).expect("seed save");

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

    #[gpui::test]
    fn switching_away_from_a_zero_linked_worktree_repo_and_back_keeps_its_worktree_row(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        git(repo_a.path(), &["init", "-b", "main"]);
        git(repo_a.path(), &["config", "user.email", "test@example.com"]);
        git(repo_a.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo_a.path().join("a.txt"), "hello\n").expect("write");
        git(repo_a.path(), &["add", "a.txt"]);
        git(repo_a.path(), &["commit", "-m", "init"]);

        let repo_b = tempfile::tempdir().expect("tempdir");
        git(repo_b.path(), &["init", "-b", "main"]);
        git(repo_b.path(), &["config", "user.email", "test@example.com"]);
        git(repo_b.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo_b.path().join("b.txt"), "hello\n").expect("write");
        git(repo_b.path(), &["add", "b.txt"]);
        git(repo_b.path(), &["commit", "-m", "init"]);

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.worktrees.len(),
                1,
                "repo A must show its own main checkout as one real worktree row right after \
                 opening, got: {:?}",
                app.worktrees
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo_path(), repo_b.path());
            assert_eq!(
                app.worktrees.len(),
                1,
                "repo B must also show one real worktree row"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_a.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.focused_repo_path(), repo_a.path());
            assert_eq!(
                app.worktrees.len(),
                1,
                "switching back to repo A must still show its one real worktree row, not lose \
                 it - got: {:?}",
                app.worktrees
            );
            assert!(
                app.worktrees[0].branch.is_some(),
                "repo A's worktree row must still carry its real current branch after \
                 switching back, got: {:?}",
                app.worktrees
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

    #[gpui::test]
    fn open_repo_in_current_window_leaves_the_previous_repos_agents_running(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();
        let repo_a_agent_id = app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                1,
                "sanity check: repo A's own initial shell agent"
            );
            app.agents.iter().next().expect("repo A's agent").id
        });

        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_b.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, cx| {
            let repo_a_agent = app
                .agents
                .iter()
                .find(|agent| agent.id == repo_a_agent_id)
                .expect(
                    "repo A's own agent must still genuinely exist - real cross-repo \
                     persistence, not merely hidden from the UI",
                );
            assert_eq!(
                repo_a_agent.cwd,
                repo_a.path(),
                "sanity check: it's still really rooted in repo A"
            );
            assert!(
                repo_a_agent.pane.read(cx).is_running(),
                "repo A's agent must still be a real, live process - not paused, not killed"
            );
            assert_eq!(
                app.agents.iter().count(),
                2,
                "repo B gets its own fresh initial shell alongside repo A's untouched one - \
                 never a replacement for it"
            );
        });

        // Switch back to repo A - the exact same agent (same `AgentId`), not a fresh respawn,
        // must be what's reachable again.
        app.update_in(cx, |app, window, cx| {
            app.open_repo_in_current_window(repo_a.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, cx| {
            assert_eq!(
                app.agents
                    .iter()
                    .filter(|agent| agent.cwd == repo_a.path())
                    .count(),
                1,
                "revisiting repo A must not spawn a redundant second shell - its own real agent \
                 was still there the whole time"
            );
            let repo_a_agent = app
                .agents
                .iter()
                .find(|agent| agent.cwd == repo_a.path())
                .expect("repo A's agent");
            assert_eq!(
                repo_a_agent.id, repo_a_agent_id,
                "switching back to repo A must find the exact same agent mid-work, not a new one"
            );
            assert!(
                repo_a_agent.pane.read(cx).is_running(),
                "still a real, live process after the round trip"
            );
            assert_eq!(
                app.agents.active_id(),
                Some(repo_a_agent_id),
                "switching back must re-activate repo A's own agent in the centre pane"
            );
        });
    }

    #[gpui::test]
    fn checkout_repo_from_rail_leaves_the_previous_repos_agents_running(cx: &mut TestAppContext) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let claude_agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::claude(),
                repo_a.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let repo_a_agent_ids: std::collections::HashSet<AgentId> = app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.iter().count(),
                2,
                "sanity check: repo A's initial shell plus the spawned Claude agent"
            );
            app.agents.iter().map(|agent| agent.id).collect()
        });

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, cx| {
            for id in &repo_a_agent_ids {
                let agent = app
                    .agents
                    .iter()
                    .find(|agent| agent.id == *id)
                    .unwrap_or_else(|| {
                        panic!(
                            "repo A's agent {id:?} must still genuinely exist after checking \
                             out repo B from the rail - real cross-repo persistence, not merely \
                             hidden from the UI"
                        )
                    });
                assert_eq!(
                    agent.cwd,
                    repo_a.path(),
                    "sanity check: still really rooted in repo A"
                );
                assert!(
                    agent.pane.read(cx).is_running(),
                    "repo A's agent {id:?} must still be a real, live process"
                );
            }
            assert!(
                app.agents.iter().any(|agent| agent.id == claude_agent_id),
                "the spawned Claude agent specifically must have survived, not just the shell"
            );
            assert_eq!(
                app.agents.iter().count(),
                2,
                "unlike `open_repo_in_current_window`, checking out a repo from the rail still \
                 does not auto-spawn a fallback shell for repo B - repo B comes up with zero \
                 agents of its own, per `checkout_repo_from_rail`'s own docs, while repo A's two \
                 real agents are the only ones left running"
            );
            assert_eq!(
                app.focused_repo_path(),
                repo_b.path(),
                "sanity check: repo B really is the focused repo after checkout"
            );
        });
    }

    #[gpui::test]
    fn checking_out_a_repo_from_the_rail_clears_the_centre_pane_instead_of_reactivating(
        cx: &mut TestAppContext,
    ) {
        let repo_a = tempfile::tempdir().expect("tempdir");
        let repo_b = tempfile::tempdir().expect("tempdir");

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        // Give repo B a real agent of its own first, and make it `Agents::active_by_cwd`'s
        // remembered tab for repo B's path - the exact state `Agents::activate_for_worktree`
        // would resurrect on a later visit. Then leave repo B (back to A) before the real
        // assertion below, so what's being tested is a genuine *re*-checkout, not a first visit.
        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });
        cx.run_until_parked();
        let repo_b_agent_id = app.update_in(cx, |app, window, cx| {
            app.agents.spawn(
                ProcessKind::Shell,
                repo_b.path().to_path_buf(),
                12.0,
                None,
                None,
                window,
                cx,
            )
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.agents.active_id(),
                Some(repo_b_agent_id),
                "sanity check: repo B's freshly spawned agent really is the active one right now"
            );
        });

        let repo_a_id = app.read_with(cx, |app, _| {
            app.repos
                .iter()
                .find(|repo| repo.path == repo_a.path())
                .expect("repo A is a known repo")
                .id
        });
        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_a_id, window, cx);
        });
        cx.run_until_parked();

        // The real assertion: re-checking out repo B, with its own agent still alive and still
        // `Agents::active_by_cwd`'s remembered tab for its path - exactly what
        // `Agents::activate_for_worktree` would resurrect - must not reactivate it.
        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, cx| {
            assert_eq!(
                app.agents.active_id(),
                None,
                "checking out repo B again must leave nothing globally active, even though it \
                 has a real, remembered agent of its own - focusing a repo is pure navigation, \
                 never a worktree selection, so the centre pane must show genuinely nothing \
                 rather than reactivating it"
            );
            let repo_b_agent = app
                .agents
                .iter()
                .find(|agent| agent.id == repo_b_agent_id)
                .expect("repo B's agent must still genuinely exist");
            assert!(
                repo_b_agent.pane.read(cx).is_running(),
                "and it must still be a real, live process - only *which* tab is shown changed, \
                 nothing about repo B's own background persistence"
            );
        });
    }

    #[gpui::test]
    fn checking_out_a_repo_from_the_rail_never_selects_a_worktree_on_its_own(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.selected, None,
                "checking out a repo must never select a worktree on the user's behalf - only \
                 a real click on a worktree row does that. An earlier version of this fix \
                 auto-selected the main worktree here, which just moved the \"repo itself is \
                 actionable\" bug one level deeper (see `Self::new_agent`'s own docs for where \
                 the real fix lives instead)"
            );
        });

        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.selected, None,
                "`Self::load_worktrees`'s own fetch must not auto-select anything either, for \
                 the identical reason"
            );
        });
    }

    #[gpui::test]
    fn checking_out_a_repo_from_the_rail_never_shows_the_previous_repos_worktrees_even_briefly(
        cx: &mut TestAppContext,
    ) {
        let repo_a = init_repo();
        let repo_b = init_repo();

        let (app, cx) = focus::palette_focus_tests::open_test_app(cx, repo_a.path().to_path_buf());
        cx.run_until_parked();

        let repo_b_id = app.update(cx, |app, cx| app.add_repo(repo_b.path().to_path_buf(), cx));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.checkout_repo_from_rail(repo_b_id, window, cx);
        });

        app.read_with(cx, |app, _| {
            assert!(
                app.worktrees.iter().all(|item| item.path != repo_a.path()),
                "repo A's own worktree must never appear in the rendered list once repo B is \
                 focused, not even for the one frame before the background fetch resolves - got \
                 {:?}",
                app.worktrees
            );
            assert!(
                app.worktrees.iter().any(|item| item.path == repo_b.path()),
                "repo B's own main worktree must be showing immediately, not waiting on the \
                 background fetch"
            );
        });
    }

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
                selected_worktree: None,
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
            app.new_agent(ProcessKind::Shell, window, cx);
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
pub(crate) mod menu_commands;
pub(crate) mod menus;
pub(crate) mod new_file;
pub(crate) mod plural;
pub(crate) mod rem_scope;
pub(crate) mod resize;
pub(crate) mod scrollbar;
pub(crate) mod scrollbar_geometry;
pub(crate) mod state;
pub(crate) mod task_pool;
pub(crate) mod widgets;
