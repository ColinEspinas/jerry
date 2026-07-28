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
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    actions, div, font, prelude::*, px, rgb, App, ClickEvent, Context, FocusHandle, KeyDownEvent,
    MouseButton, Task, Window, WindowControlArea,
};
use wt_core::diff::{DiffBase, DiffFile, DiffLineKind, FileChangeStatus};

use crate::file_tree::{self, FileTreeEntry};
use crate::rail::{
    self, ProjectChild, RailMode, SessionRow, StatusGroup, WorktreeEntry, WorktreeNote,
};
use crate::sessions::{Session, SessionId, SessionKind, Sessions};
use crate::status::{self, Status};
use crate::theme;
use crate::work_surface;
use crate::worktrees::{self, WorktreeItem};

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
actions!(app, [NewSession]);

/// How often the rail's real background status refresh (real `wt_core::diff::
/// diff_against_base` and `wt_core::is_dirty`/`merge_status_against_base` calls, via
/// `crate::rail::compute_status_snapshot`) re-runs. Coarser than `crate::terminal_pane`'s
/// 33ms output-drain poll: those are cheap channel `try_recv`s, while this tick spawns real
/// `git` child processes and reads the object database per distinct worktree/session path -
/// frequent enough that the rail's status/diff numbers feel live, not so frequent that a
/// handful of open sessions turns into a constant stream of `git` spawns.
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// See the comment at its use site in `render_file_tree` for why this exists.
const MAX_RENDERED_FILE_ENTRIES: usize = 500;

/// Cap on how many changed files the diff view turns into rendered elements, independent of
/// `wt_core::diff`'s own `MAX_FILES` cap (300) on the *loaded* diff. Mirrors
/// `MAX_RENDERED_FILE_ENTRIES` above for the same reason: `wt_core::diff` can hand back up
/// to 300 files, each carrying its own hunk lines on top, and laying all of that out as
/// GPUI divs on every render is the same kind of foreground-executor stall documented at
/// `MAX_RENDERED_FILE_ENTRIES`'s use site, just with a much larger multiplier.
const MAX_RENDERED_DIFF_FILES: usize = 40;

/// Cap on how many hunk lines a single file's diff renders, independent of `wt_core::diff`'s
/// own per-file `MAX_HUNK_LINES_PER_FILE` cap (2000) on loaded data. Same reasoning as
/// `MAX_RENDERED_DIFF_FILES`: a single enormous file (e.g. a generated lockfile that slipped
/// past the loaded-data cap) shouldn't be allowed to blow up render time on its own.
const MAX_RENDERED_DIFF_LINES_PER_FILE: usize = 300;

/// Which real data source the right sidebar currently shows for the selected worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RightSidebarView {
    Files,
    Diff,
}

/// The outcome of the most recent (or in-flight) `wt_core::diff::diff_against_base` call for
/// [`AdeApp::diff_root`]. Kept separate from [`DiffBase`] (rather than wrapping it in an
/// `Option`/`Result` at the call site) so "still computing" is a first-class, renderable
/// state rather than reusing an empty/default value that could be mistaken for "no changes".
enum DiffLoadState {
    Loading,
    Loaded(DiffBase),
    Error(String),
}

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
    _load_worktrees_task: Option<Task<()>>,
    _load_file_tree_task: Option<Task<()>>,
    _load_diff_task: Option<Task<()>>,
    _status_poll_task: Option<Task<()>>,
    _disk_usage_task: Option<Task<()>>,
    _prune_task: Option<Task<()>>,
}

impl AdeApp {
    pub fn new(repo_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            file_tree_root: repo_path.clone(),
            diff_root: repo_path.clone(),
            repo_path: repo_path.clone(),
            worktrees: Vec::new(),
            worktrees_error: None,
            selected: None,
            sessions: Sessions::new(),
            file_tree: Vec::new(),
            file_tree_error: None,
            right_sidebar_view: RightSidebarView::Files,
            diff_state: DiffLoadState::Loading,
            title_bar_move_armed: false,
            rail_mode: RailMode::default(),
            filter_query: String::new(),
            filter_focus_handle: cx.focus_handle(),
            diff_cache: HashMap::new(),
            worktree_notes: HashMap::new(),
            disk_usage: None,
            prune_status: None,
            prune_confirm_armed: false,
            _load_worktrees_task: None,
            _load_file_tree_task: None,
            _load_diff_task: None,
            _status_poll_task: None,
            _disk_usage_task: None,
            _prune_task: None,
        };
        // A fresh window shouldn't open with zero tabs and no way to see anything running -
        // start with one real shell in the repo root, exactly like step 3's single terminal
        // did, except now it's a tab like any other rather than the only pane that can
        // exist.
        this.sessions
            .spawn(SessionKind::Shell, repo_path.clone(), cx);
        this.load_worktrees(cx);
        this.load_file_tree(repo_path.clone(), cx);
        this.load_diff(repo_path, cx);
        this.start_status_polling(cx);
        this
    }

    fn load_worktrees(&mut self, cx: &mut Context<Self>) {
        let repo_path = self.repo_path.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { wt_core::list_worktrees(&repo_path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(results) => {
                        this.worktrees = worktrees::build_worktree_items(results);
                        this.worktrees_error = None;
                    }
                    Err(err) => {
                        this.worktrees = Vec::new();
                        this.worktrees_error = Some(err.to_string());
                    }
                }
                this.load_disk_usage(cx);
                cx.notify();
            });
        });
        self._load_worktrees_task = Some(task);
    }

    /// Recomputes [`Self::disk_usage`] from the current real worktree list, offloaded to the
    /// background executor - see `crate::rail::disk_usage_bytes`'s docs for the real, bounded
    /// `std::fs` walk this sums across every readable worktree. Run once per worktree-list
    /// load (not on the 3s status-poll cadence - a `std::fs` walk is real per-file I/O, and
    /// re-walking every worktree's entire tree every 3s would be needless cost for a number
    /// that only meaningfully changes when a worktree is added, removed, or its files
    /// change).
    fn load_disk_usage(&mut self, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .worktrees
            .iter()
            .filter(|item| item.error.is_none())
            .map(|item| item.path.clone())
            .collect();

        let task = cx.spawn(async move |this, cx| {
            let usage = cx
                .background_executor()
                .spawn(async move {
                    let mut total = 0u64;
                    let mut truncated = false;
                    for path in paths {
                        let (bytes, path_truncated) = rail::disk_usage_bytes(&path);
                        total += bytes;
                        truncated |= path_truncated;
                    }
                    (total, truncated)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.disk_usage = Some(usage);
                cx.notify();
            });
        });
        self._disk_usage_task = Some(task);
    }

    fn load_file_tree(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.file_tree_root = root.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move { file_tree::build_file_tree(&root) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(entries) => {
                        this.file_tree = entries;
                        this.file_tree_error = None;
                    }
                    Err(err) => {
                        this.file_tree = Vec::new();
                        this.file_tree_error = Some(err.to_string());
                    }
                }
                cx.notify();
            });
        });
        self._load_file_tree_task = Some(task);
    }

    /// Loads (or reloads) the real diff of `root` against its detected base branch, per
    /// `wt_core::diff`'s docs. Offloaded to `cx.background_executor()` for the same reason
    /// `load_worktrees`/`load_file_tree` are: `diff_against_base` performs blocking I/O
    /// (`gix` reads plus a spawned `git diff` child process) and must never run on the GPUI
    /// foreground thread.
    fn load_diff(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.diff_root = root.clone();
        self.diff_state = DiffLoadState::Loading;
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let root = root.clone();
                    async move { wt_core::diff::diff_against_base(&root) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.diff_state = match result {
                    Ok(base) => DiffLoadState::Loaded(base),
                    Err(err) => DiffLoadState::Error(err.to_string()),
                };
                cx.notify();
            });
        });
        self._load_diff_task = Some(task);
    }

    /// The worktree a *new* session should be spawned into: the selected worktree's real
    /// path if one is selected and readable, otherwise the repo root - see the module docs'
    /// "Sessions/tabs" section for why this is resolved at spawn time rather than tracked as
    /// a per-tab "current worktree".
    fn active_session_cwd(&self) -> PathBuf {
        match self.selected.and_then(|index| self.worktrees.get(index)) {
            Some(item) if item.error.is_none() => item.path.clone(),
            _ => self.repo_path.clone(),
        }
    }

    fn select_worktree(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.worktrees.get(index) else {
            return;
        };
        if item.error.is_some() {
            // An unreadable entry has no usable path; nothing to select into.
            return;
        }
        let path = item.path.clone();
        self.selected = Some(index);
        // Any other rail interaction disarms a pending prune confirmation - see
        // `Self::request_prune`'s docs. Browsing to a different worktree is exactly the kind
        // of "I did something else" that must not let a stale armed click land later.
        self.prune_confirm_armed = false;
        self.load_file_tree(path.clone(), cx);
        self.load_diff(path, cx);
        cx.notify();
    }

    /// Switches which real data source the right sidebar shows. Switching *to* the Diff view
    /// always recomputes it (`load_diff`, not just `cx.notify()`) rather than showing
    /// whatever was last loaded: the core workflow this feature exists for is "run an agent
    /// in a terminal tab, then check the diff", and a stale snapshot captured back when the
    /// worktree was first selected would silently hide exactly the changes just made -
    /// worse than an obviously-loading state.
    fn set_right_sidebar_view(&mut self, view: RightSidebarView, cx: &mut Context<Self>) {
        self.right_sidebar_view = view;
        if view == RightSidebarView::Diff {
            self.load_diff(self.diff_root.clone(), cx);
        } else {
            cx.notify();
        }
    }

    fn new_session(&mut self, kind: SessionKind, cx: &mut Context<Self>) {
        let cwd = self.active_session_cwd();
        self.sessions.spawn(kind, cwd, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    fn handle_new_session_action(
        &mut self,
        _action: &NewSession,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session(SessionKind::Shell, cx);
    }

    /// Selects a worktree by its real path (rather than an index into
    /// [`Self::worktrees`], which project-mode rows don't carry) - used by a plain worktree
    /// row's click handler in "by project" mode. Falls back to doing nothing if the path
    /// isn't currently in the loaded worktree list (e.g. a stale click racing a reload).
    fn select_worktree_by_path(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        if let Some(index) = self.worktrees.iter().position(|item| item.path == path) {
            self.select_worktree(index, cx);
        }
    }

    /// Activates session `id`'s tab and, if it maps to a currently-listed worktree, also
    /// selects that worktree (keeping the right-hand file tree/diff panel in sync with the
    /// session the user just clicked) - see the module docs for why this double duty is a
    /// deliberate integration point rather than the rail owning its own separate notion of
    /// "current worktree": the right sidebar is still driven by [`Self::selected`], since
    /// Zone 2/3 (which the design's state model has as `focused_session`-driven) hasn't been
    /// rebuilt yet.
    fn select_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.sessions.set_active(id);
        self.prune_confirm_armed = false;
        let cwd = self
            .sessions
            .iter()
            .find(|session| session.id == id)
            .map(|session| session.cwd.clone());
        if let Some(cwd) = cwd {
            if let Some(index) = self.worktrees.iter().position(|item| item.path == cwd) {
                if self.selected != Some(index) {
                    self.select_worktree(index, cx);
                    return;
                }
            }
        }
        cx.notify();
    }

    /// Derives the real [`Status`] for one live session - the single source of truth both
    /// [`Self::build_session_rows`] (the rail) and Zone 2's restyle (the context bar's status
    /// pill, and the CLI/terminal pane header/footer) read, so the rail and the work surface
    /// can never disagree about a session's status. Mirrors `Self::build_session_rows`'s own
    /// prior inline signal-gathering exactly - factored out once a second call site (Zone 2)
    /// needed the identical logic, rather than a second, independently-drifting copy of it.
    fn session_status(&self, session: &Session, cx: &App) -> Status {
        let pane = session.pane.read(cx);
        let signal = if pane.is_running() {
            status::ProcessSignal::Running {
                idle: pane.idle_duration().unwrap_or_default(),
            }
        } else if let Some(exit) = pane.exit_status() {
            status::ProcessSignal::Exited {
                success: exit.success(),
            }
        } else if pane.spawn_error().is_some() {
            // A process that never started is a real failure the status derivation should
            // surface, even though it has no `ExitStatus` of its own to report.
            status::ProcessSignal::Exited { success: false }
        } else {
            status::ProcessSignal::NoProcess
        };
        let has_diff = self
            .diff_cache
            .get(&session.cwd)
            .map(|summary| summary.has_changes)
            .unwrap_or(false);
        status::derive_status(session.kind, signal, has_diff)
    }

    /// The context bar's real `Archive` action, and the idle-status footer's own `Archive`
    /// action (`design_handoff_jerry_ade/README.md`'s Session context bar spec: "`Merge`
    /// (outline) · `Archive` (ghost)") - closes the tab via the already-real `Sessions::
    /// close` (which deterministically tears down the real child process - see that method's
    /// docs), exactly like the tab strip's own `×`. Not a placeholder: this is the one
    /// context-bar action with real, already-existing backing logic - `Merge` has none (see
    /// `render_merge_button`'s docs for why it's honestly disabled instead of wired to
    /// something fake).
    fn archive_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        self.sessions.close(id, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// The surface footer's real `Interrupt ⌃C` action - sends a real `Ctrl-C` to the
    /// session's own pty via `TerminalPane::interrupt`, exactly as if the user had focused the
    /// pane and pressed Ctrl-C themselves.
    fn interrupt_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let pane = session.pane.clone();
        pane.update(cx, |pane, cx| pane.interrupt(cx));
    }

    /// The surface footer's real `Retry ⌘R` (failed sessions) / `Resume ⌘⏎` (idle sessions)
    /// action. This app has no saved-session resumability to actually resume *from* (see
    /// `crate::work_surface::pty_state_label`'s docs on the same gap: no `detached ·
    /// resumable` state exists here) - the real, honest equivalent implemented here is: close
    /// this tab, then spawn a fresh session of the same kind into the same worktree, exactly
    /// as if the user had clicked "New ... Session" again themselves. A real action (the old
    /// process is genuinely torn down, a new one genuinely started), just not literally
    /// "resume where it left off" - `crate::work_surface::ActionKind::Respawn`'s docs name
    /// this same trade-off.
    fn respawn_session(&mut self, id: SessionId, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return;
        };
        let kind = session.kind;
        let cwd = session.cwd.clone();
        self.sessions.close(id, cx);
        self.sessions.spawn(kind, cwd, cx);
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// The surface footer's real `Open terminal` action - finds an already-open real `Shell`
    /// session in the same worktree and selects it, or spawns one if none exists yet. Real,
    /// minimal: this app's session model has no notion of "the terminal view of *this*
    /// session" (each session is its own independent tab/process - see `crate::sessions`'
    /// module docs), so "open terminal" here means "get me a shell in this worktree", the same
    /// real capability the rail's own "+ New Shell" button already provides.
    fn open_companion_terminal(&mut self, cwd: PathBuf, cx: &mut Context<Self>) {
        let existing = self
            .sessions
            .iter()
            .find(|session| session.kind == SessionKind::Shell && session.cwd == cwd)
            .map(|session| session.id);
        match existing {
            Some(id) => self.select_session(id, cx),
            None => {
                self.sessions.spawn(SessionKind::Shell, cwd, cx);
                self.prune_confirm_armed = false;
                cx.notify();
            }
        }
    }

    fn toggle_rail_mode(&mut self, cx: &mut Context<Self>) {
        self.rail_mode = self.rail_mode.toggled();
        self.prune_confirm_armed = false;
        cx.notify();
    }

    /// Types (or backspaces/clears) into [`Self::filter_query`] - a small, deliberately
    /// minimal hand-rolled text field (append/backspace only, no cursor positioning or
    /// selection), mirroring `crate::terminal_pane::keystroke_to_bytes`'s own "small,
    /// deliberate subset" scope cut rather than porting `vendor/zed/crates/gpui/examples/
    /// input.rs`'s full `EntityInputHandler` (IME marked-text, mouse selection, clipboard) -
    /// judged out of scope for a single filter row. Modified keystrokes (⌘, ⌃, ⌥) are left
    /// unhandled and keep propagating, so app-level shortcuts (e.g. ⌘N) still reach their
    /// bindings while this field has focus.
    fn handle_filter_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        let changed = match keystroke.key.as_str() {
            "backspace" => self.filter_query.pop().is_some(),
            "escape" => {
                let had_text = !self.filter_query.is_empty();
                self.filter_query.clear();
                had_text
            }
            _ => match keystroke.key_char.as_deref() {
                Some(text) if !text.is_empty() => {
                    self.filter_query.push_str(text);
                    true
                }
                _ => false,
            },
        };
        if changed {
            self.prune_confirm_armed = false;
            cx.notify();
            cx.stop_propagation();
        }
    }

    /// Builds the rail's real per-session rows from live state: each session's
    /// `TerminalPane` (process signal, question preview - see `crate::terminal_pane`'s new
    /// `is_running`/`idle_duration`/`exit_status`/`spawn_error`/`visible_text_lines`
    /// getters), the matching worktree's real branch name, and the real diff summary from
    /// [`Self::diff_cache`] (refreshed by the periodic task started in `Self::new`). No
    /// field here is fabricated or hardcoded - a session with no diff data yet simply shows
    /// `0`/`0` until the next status-poll tick fills it in.
    fn build_session_rows(&self, cx: &App) -> Vec<SessionRow> {
        self.sessions
            .iter()
            .map(|session| {
                let status_value = self.session_status(session, cx);
                let pane = session.pane.read(cx);
                let diff = self.diff_cache.get(&session.cwd).copied();

                let branch = self
                    .worktrees
                    .iter()
                    .find(|item| item.path == session.cwd)
                    .and_then(|item| item.branch.clone());

                let question_preview = if status_value == Status::Ask {
                    pane.visible_text_lines()
                        .into_iter()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                } else {
                    None
                };

                let title = match session.cwd.file_name() {
                    Some(name) => name.to_string_lossy().into_owned(),
                    None => session.cwd.display().to_string(),
                };

                SessionRow {
                    id: session.id,
                    kind: session.kind,
                    title,
                    cwd: session.cwd.clone(),
                    status: status_value,
                    branch,
                    add: diff.map(|summary| summary.add).unwrap_or(0),
                    del: diff.map(|summary| summary.del).unwrap_or(0),
                    question_preview,
                    exit_code: pane.exit_status().map(|status| status.exit_code()),
                }
            })
            .collect()
    }

    /// Builds the real "by project" worktree list: **every** worktree `wt_core::
    /// list_worktrees` reported, including ones that failed to read - `crate::worktrees::
    /// WorktreeItem`'s own docs say a per-entry error is kept in the list "so the problem is
    /// visible rather than the entry silently vanishing", and filtering them out here would
    /// defeat that intent (a real Phase-A behavior this rewrite must not regress). An
    /// errored item gets a [`WorktreeEntry`] with `error: Some(..)` and an empty note
    /// (nothing real to compute a clean/merged state from); `crate::root::AdeApp::
    /// render_worktree_note_row` renders that as a visible, non-interactive error row rather
    /// than a normal clickable one.
    ///
    /// Readable entries get their real clean/merged note from [`Self::worktree_notes`]
    /// (refreshed by the same periodic task as [`Self::diff_cache`]) - defaulting to an
    /// "unknown yet" note (`clean: None, merge: None`) for one the background snapshot
    /// hasn't reached yet, rather than guessing.
    fn build_worktree_entries(&self) -> Vec<WorktreeEntry> {
        self.worktrees
            .iter()
            .map(|item| {
                if let Some(error) = &item.error {
                    return WorktreeEntry {
                        path: item.path.clone(),
                        label: item.label.clone(),
                        branch: None,
                        note: WorktreeNote {
                            is_main: false,
                            clean: None,
                            merge: None,
                            is_locked: false,
                        },
                        error: Some(error.clone()),
                    };
                }

                let note = self
                    .worktree_notes
                    .get(&item.path)
                    .cloned()
                    .unwrap_or(WorktreeNote {
                        is_main: item.is_main,
                        clean: None,
                        merge: None,
                        is_locked: item.is_locked,
                    });
                WorktreeEntry {
                    path: item.path.clone(),
                    label: item.label.clone(),
                    branch: item.branch.clone(),
                    note,
                    error: None,
                }
            })
            .collect()
    }

    /// Starts the rail's periodic real-status background refresh (see
    /// [`STATUS_POLL_INTERVAL`]'s docs). Every tick: snapshots the current worktree paths and
    /// open sessions' cwds on the foreground thread (cheap, no I/O), computes a real
    /// [`rail::StatusSnapshot`] on the background executor (real `git`/`gix` calls - see
    /// `rail::compute_status_snapshot`'s docs), then writes the result back into
    /// [`Self::diff_cache`]/[`Self::worktree_notes`] on the foreground thread. Mirrors the
    /// same "gather on foreground, compute on background, write back on foreground" shape
    /// [`Self::load_worktrees`]/[`Self::load_diff`] already use.
    fn start_status_polling(&mut self, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(STATUS_POLL_INTERVAL).await;

            let Ok((worktrees, diff_paths)) = this.update(cx, |this, _cx| {
                let worktrees: Vec<rail::WorktreeQuery> = this
                    .worktrees
                    .iter()
                    .filter(|item| item.error.is_none())
                    .map(|item| rail::WorktreeQuery {
                        path: item.path.clone(),
                        is_main: item.is_main,
                        is_locked: item.is_locked,
                    })
                    .collect();
                let diff_paths: Vec<PathBuf> = this
                    .sessions
                    .iter()
                    .map(|session| session.cwd.clone())
                    .collect();
                (worktrees, diff_paths)
            }) else {
                break;
            };

            let snapshot = cx
                .background_executor()
                .spawn(async move { rail::compute_status_snapshot(&worktrees, &diff_paths) })
                .await;

            let updated = this.update(cx, |this, cx| {
                this.diff_cache = snapshot.diffs;
                this.worktree_notes = snapshot.worktree_notes;
                cx.notify();
            });
            if updated.is_err() {
                break;
            }
        });
        self._status_poll_task = Some(task);
    }

    /// The rail footer's real `prune` action: removes every currently-known real prune
    /// candidate (not the main checkout, clean, merged - see [`rail::WorktreeNote::
    /// is_prunable`]) via the real, already-tested `wt_core::remove_worktree` (with
    /// `force: false`, so its own dirty-tree refusal still guards against a race between the
    /// last status snapshot and this click), then reloads the real worktree list. Real
    /// functionality, not a decorative label: this can and does delete real directories on
    /// disk.
    /// The real prune candidate list: every worktree that is a prune candidate on its own
    /// merits ([`rail::is_prunable`]) **and** has no live session currently running with its
    /// cwd inside it - see [`rail::prunable_worktree_paths`]'s docs for why that second
    /// condition is not optional. Shared by the footer's displayed count and the actual
    /// removal, so what's shown always matches what a click will really do.
    fn prunable_worktree_paths(&self) -> Vec<PathBuf> {
        let worktree_paths: Vec<PathBuf> = self
            .worktrees
            .iter()
            .filter(|item| item.error.is_none())
            .map(|item| item.path.clone())
            .collect();
        let live_session_cwds: HashSet<PathBuf> = self
            .sessions
            .iter()
            .map(|session| session.cwd.clone())
            .collect();
        rail::prunable_worktree_paths(&worktree_paths, &self.worktree_notes, &live_session_cwds)
    }

    /// The footer `prune` button's click handler. Destructive, so this is deliberately a
    /// two-click confirmation, not a single unconfirmed click: the first click only arms
    /// [`Self::prune_confirm_armed`] and changes the button's own label (real, visible
    /// feedback - see `Self::render_rail_footer`), and does not touch the filesystem at all.
    /// Only a *second* click while already armed calls [`Self::execute_prune`]. This matters
    /// beyond the design's own footer-label spec: `wt_core::is_dirty` correctly follows
    /// git's own ignored-file semantics, so a "clean" worktree can still hold real,
    /// gitignored state (secrets, build artifacts, uncommitted-but-ignored work) that a
    /// single misclick would otherwise destroy silently, for potentially several worktrees
    /// at once.
    fn request_prune(&mut self, cx: &mut Context<Self>) {
        let candidates = self.prunable_worktree_paths();

        if candidates.is_empty() {
            self.prune_confirm_armed = false;
            self.prune_status = Some("nothing to prune".to_string());
            cx.notify();
            return;
        }

        if !self.prune_confirm_armed {
            self.prune_confirm_armed = true;
            self.prune_status = Some(format!(
                "click prune again to remove {} worktree(s)",
                candidates.len()
            ));
            cx.notify();
            return;
        }

        self.prune_confirm_armed = false;
        self.execute_prune(candidates, cx);
    }

    /// Actually removes `candidates` via the real, already-tested `wt_core::remove_worktree`.
    /// Only ever called once [`Self::request_prune`]'s confirmation step has been satisfied,
    /// and only with paths [`Self::prunable_worktree_paths`] itself produced (never the main
    /// checkout, never a locked worktree, never one with a live session).
    fn execute_prune(&mut self, candidates: Vec<PathBuf>, cx: &mut Context<Self>) {
        let repo_path = self.repo_path.clone();
        self.prune_status = Some(format!("pruning {} worktree(s)...", candidates.len()));
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn({
                    let repo_path = repo_path.clone();
                    async move {
                        let mut removed = 0usize;
                        let mut errors = Vec::new();
                        for path in candidates {
                            match wt_core::remove_worktree(&repo_path, &path, false) {
                                Ok(()) => removed += 1,
                                Err(err) => errors.push(format!("{}: {err}", path.display())),
                            }
                        }
                        (removed, errors)
                    }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                let (removed, errors) = outcome;
                this.prune_status = Some(if errors.is_empty() {
                    format!("pruned {removed} worktree(s)")
                } else {
                    format!(
                        "pruned {removed}; {} failed: {}",
                        errors.len(),
                        errors.join("; ")
                    )
                });
                this.load_worktrees(cx);
                cx.notify();
            });
        });
        self._prune_task = Some(task);
    }

    /// The whole session rail (`design_handoff_jerry_ade/README.md`'s Zone 1): header,
    /// filter row, the real scrollable session/worktree list, and the footer - see the
    /// README's "Rail chrome" section for the exact band heights this composes
    /// (`theme::band::{RAIL_HEADER,FILTER_ROW,SURFACE_FOOTER}`).
    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("session-rail")
            .flex()
            .flex_col()
            .size_full()
            .child(self.render_rail_header(cx))
            .child(self.render_rail_filter_row(cx))
            .when_some(self.render_worktrees_error_banner(), |el, banner| {
                el.child(banner)
            })
            .child(
                div()
                    .id("session-rail-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_rail_list(cx)),
            )
            .child(self.render_rail_footer(cx))
    }

    /// A real, visible error banner for [`Self::worktrees_error`] - a real Phase-A behavior
    /// (`wt_core::list_worktrees` failing outright, e.g. a corrupt repository) this rewrite
    /// must not silently drop: the old sidebar returned early with exactly this message; the
    /// rail shows it as a standing banner instead (rather than replacing the whole session
    /// list) so real, already-open sessions stay visible and usable even when the worktree
    /// listing itself is broken.
    fn render_worktrees_error_banner(&self) -> Option<impl IntoElement> {
        let error = self.worktrees_error.as_ref()?;
        Some(
            div()
                .id("rail-worktrees-error")
                .flex_none()
                .px(px(10.0))
                .py(px(6.0))
                .bg(theme::status::FAIL_BG)
                .border_b_1()
                .border_color(theme::border::RAIL_INNER)
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::status::FAIL)
                .child(format!("failed to list worktrees: {error}")),
        )
    }

    /// Header 36 (`Sessions` label, grouping toggle, `+`/⌘N) - README's "Rail chrome".
    fn render_rail_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rail-header")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .h(theme::band::RAIL_HEADER)
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(10.0))
                    .text_color(theme::text::FAINT)
                    .child("SESSIONS"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(self.render_rail_mode_toggle(cx))
                    .child(self.render_new_session_button(cx)),
            )
    }

    /// The `by urgency ▾ / by project ▾` control.
    fn render_rail_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rail-mode-toggle")
            .cursor_pointer()
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .font(font(theme::font::MONO))
            .text_size(px(10.0))
            .text_color(theme::text::DIM)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child(format!("{} \u{25be}", self.rail_mode.label()))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.toggle_rail_mode(cx);
            }))
    }

    /// The `+` control with its ⌘N keycap pair - spawns a real new shell session (see
    /// [`NewSession`]'s docs for the judgment call on the keybinding side of this).
    fn render_new_session_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rail-new-session")
            .flex()
            .items_center()
            .gap(px(4.0))
            .cursor_pointer()
            .px(px(6.0))
            .py(px(2.0))
            .rounded(theme::radius::CHIP)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child(
                div()
                    .text_color(theme::text::DIM)
                    .text_size(px(11.0))
                    .child("+"),
            )
            .child(render_keycap_pair("\u{2318}", "N"))
            .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                this.new_session(SessionKind::Shell, cx);
            }))
    }

    /// Filter row 30: `/` plus the real typed query, or the placeholder text when empty -
    /// see [`Self::handle_filter_key_down`] for the (deliberately minimal) text input.
    fn render_rail_filter_row(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_query = !self.filter_query.is_empty();

        div()
            .id("rail-filter-row")
            .track_focus(&self.filter_focus_handle)
            .on_key_down(cx.listener(Self::handle_filter_key_down))
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                window.focus(&this.filter_focus_handle, cx);
            }))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .h(theme::band::FILTER_ROW)
            .border_b_1()
            .border_color(theme::border::RAIL_INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child("/"),
            )
            .child(
                div()
                    .flex_1()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(if has_query {
                        theme::text::DIM
                    } else {
                        theme::text::GHOST
                    })
                    .child(if has_query {
                        self.filter_query.clone()
                    } else {
                        "filter sessions".to_string()
                    }),
            )
    }

    /// Dispatches to the real urgency- or project-grouped list, per [`Self::rail_mode`].
    /// Builds [`SessionRow`]s fresh from live state every render (cheap: no I/O, just field
    /// reads plus the cached [`Self::diff_cache`]/[`Self::worktree_notes`] snapshots) - see
    /// [`Self::build_session_rows`]'s docs.
    fn render_rail_list(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let rows = self.build_session_rows(cx);
        match self.rail_mode {
            RailMode::Urgency => self.render_urgency_list(&rows, cx),
            RailMode::Project => self.render_project_list(&rows, cx),
        }
    }

    fn render_urgency_list(&self, rows: &[SessionRow], cx: &mut Context<Self>) -> gpui::AnyElement {
        let filtered: Vec<SessionRow> = rail::filter_sessions(rows, &self.filter_query)
            .into_iter()
            .cloned()
            .collect();
        let groups = rail::group_by_urgency(&filtered);

        if groups.is_empty() {
            return self.render_rail_empty_message(if rows.is_empty() {
                "no sessions open"
            } else {
                "no sessions match this filter"
            });
        }

        let mut list = div().id("rail-urgency-groups").flex().flex_col();
        for group in &groups {
            list = list.child(self.render_status_group(group, cx));
        }
        list.into_any_element()
    }

    fn render_rail_empty_message(&self, message: &'static str) -> gpui::AnyElement {
        div()
            .flex()
            .flex_col()
            .p(px(12.0))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child(message),
            )
            .into_any_element()
    }

    /// One urgency group: the 5×5 status-colour square + uppercase label + count header
    /// row, then every session row in that status.
    fn render_status_group(&self, group: &StatusGroup, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("status-group", group.status.urgency_rank() as u64))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(12.0))
                    .py(px(5.0))
                    .child(div().w(px(5.0)).h(px(5.0)).bg(group.status.color()))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(9.5))
                            .text_color(theme::text::FAINT)
                            .child(group.status.label().to_uppercase()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(9.5))
                            .text_color(theme::text::GHOST)
                            .child(group.rows.len().to_string()),
                    ),
            )
            .children(
                group
                    .rows
                    .iter()
                    .map(|row| self.render_session_row(row, 0, cx)),
            )
    }

    /// "By project" mode: a single project header (this app manages exactly one repository -
    /// see the module docs on why multi-project support is out of scope) followed by every
    /// worktree as a child row, indented, each either a real session row or a real
    /// session-less worktree row - see [`rail::build_project_children`]'s docs for why every
    /// worktree appears here, not just ones with an open session.
    fn render_project_list(&self, rows: &[SessionRow], cx: &mut Context<Self>) -> gpui::AnyElement {
        let worktrees = self.build_worktree_entries();
        let children = rail::build_project_children(&worktrees, rows);
        let filtered = rail::filter_project_children(&children, &self.filter_query);

        if filtered.is_empty() {
            return self.render_rail_empty_message(if children.is_empty() {
                "no worktrees found"
            } else {
                "no worktrees match this filter"
            });
        }

        let project_name = self
            .repo_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repo_path.display().to_string());
        let project_branch = self
            .worktrees
            .iter()
            .find(|item| item.is_main)
            .and_then(|item| item.branch.clone());
        let dots = rail::status_dot_cluster(&children);
        let worktree_count = worktrees.len();

        let mut list = div().id("rail-project").flex().flex_col();
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(12.0))
                .h(px(27.0))
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme::text::STRONG)
                        .child(project_name),
                )
                .when_some(project_branch, |el, branch| {
                    el.child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(theme::text::GHOST)
                            .child(branch),
                    )
                })
                .child(div().flex_1())
                .child(
                    div().flex().items_center().gap(px(3.0)).children(
                        dots.into_iter()
                            .map(|status| div().w(px(5.0)).h(px(5.0)).bg(status.color())),
                    ),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(9.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("{worktree_count} wt")),
                ),
        );

        for (index, child) in filtered.into_iter().enumerate() {
            list = list.child(self.render_project_child(child, index, cx));
        }

        list.into_any_element()
    }

    /// One indented child row under the project header, with a 1px vertical spine (README:
    /// "indented 16 with a 1px `#1e2225` vertical spine"). `index` is only used to keep
    /// element ids unique for the degenerate case of two error'd `WorktreeEntry`s sharing the
    /// same (empty) path - see `Self::render_worktree_note_row`'s docs.
    fn render_project_child(
        &self,
        child: &ProjectChild,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let row: gpui::AnyElement = match child {
            ProjectChild::Session(session_row) => self
                .render_session_row(session_row, 0, cx)
                .into_any_element(),
            ProjectChild::Worktree(entry) => self
                .render_worktree_note_row(entry, index, cx)
                .into_any_element(),
        };

        div()
            .flex()
            .pl(px(16.0))
            .border_l_1()
            .border_color(theme::border::ZONE)
            .child(row)
    }

    /// A session-less worktree row in "by project" mode - real path/branch, real
    /// `checkout · clean` / `merged HH:MM · prunable` note (see [`rail::WorktreeNote::
    /// label`]).
    fn render_worktree_note_row(
        &self,
        entry: &WorktreeEntry,
        index: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = format!("worktree-row-{index}-{}", entry.path.display());

        if let Some(error) = &entry.error {
            // A real error row, per `crate::worktrees::WorktreeItem`'s documented intent:
            // visible, not silently dropped - and deliberately not clickable (an errored
            // entry has no usable, real path to select into).
            return div()
                .id(id)
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .px(px(10.0))
                .py(px(6.0))
                .child(
                    div()
                        .font(font(theme::font::SANS))
                        .text_size(px(12.0))
                        .text_color(theme::status::FAIL)
                        .child(entry.label.clone()),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::status::FAIL)
                        .child(error.clone()),
                );
        }

        let path = entry.path.clone();
        div()
            .id(id)
            .cursor_pointer()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .px(px(10.0))
            .py(px(6.0))
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_worktree_by_path(&path, cx);
            }))
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .text_size(px(12.0))
                    .text_color(theme::text::BODY)
                    .child(entry.label.clone()),
            )
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(entry.note.label()),
            )
    }

    /// One session row, exactly per the README's spec: agent badge, title, meta, second line
    /// (status dot + branch + stat), and a question-preview card for waiting sessions.
    /// `indent` is currently always `0` (project mode already indents the whole child row via
    /// [`Self::render_project_child`]'s spine) - kept as a parameter so a future nested
    /// grouping doesn't need to change this method's signature.
    fn render_session_row(
        &self,
        row: &SessionRow,
        indent: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let is_selected = self.sessions.active_id() == Some(row.id);
        let (badge_fg, badge_bg) = work_surface::agent_tint(row.kind);

        let title_color = if is_selected {
            theme::text::SELECTED
        } else if row.status == Status::Idle {
            theme::text::DIMMER
        } else {
            theme::text::BODY
        };

        let (meta_text, meta_color) = match row.status {
            Status::Ask => ("waiting".to_string(), theme::status::ASK_CARD_FG),
            Status::Fail => ("failed".to_string(), theme::text::GHOST),
            Status::Review => ("ready".to_string(), theme::text::GHOST),
            Status::Run => ("running".to_string(), theme::text::GHOST),
            Status::Idle => ("idle".to_string(), theme::text::GHOST),
        };

        let stat_text = if row.status == Status::Fail {
            row.exit_code
                .map(|code| format!("exit {code}"))
                .unwrap_or_else(|| "failed".to_string())
        } else if row.add > 0 || row.del > 0 {
            format!("+{} \u{2212}{}", row.add, row.del)
        } else {
            String::new()
        };
        let stat_color = if row.status == Status::Fail {
            theme::button::DANGER_FG
        } else {
            theme::text::GHOST
        };

        let id = row.id;
        let mut container = div()
            .id(("session-row", id))
            .cursor_pointer()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .pl(px(12.0 + indent as f32 * 16.0))
            .pr(px(10.0))
            .pt(px(6.0))
            .pb(px(7.0))
            .border_l(px(2.0))
            .border_color(row.status.color())
            .when(is_selected, |el| el.bg(theme::surface::ROW_SELECTED))
            .when(!is_selected, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER))
            })
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.select_session(id, cx);
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme::radius::CHIP)
                            .bg(badge_bg)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(9.0))
                            .text_color(badge_fg)
                            .child(work_surface::agent_initial(row.kind)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::SANS))
                            .text_size(px(12.0))
                            .text_color(title_color)
                            .child(row.title.clone()),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(9.5))
                            .text_color(meta_color)
                            .child(meta_text),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pt(px(2.0))
                    .child(div().w(px(4.0)).h(px(4.0)).bg(row.status.color()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.5))
                            .text_color(if is_selected {
                                theme::text::DIM
                            } else {
                                theme::text::FAINTER
                            })
                            .child(
                                row.branch
                                    .clone()
                                    .unwrap_or_else(|| "(detached)".to_string()),
                            ),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.0))
                            .text_color(stat_color)
                            .child(stat_text),
                    ),
            );

        if let Some(preview) = &row.question_preview {
            container = container.child(
                div()
                    .mt(px(4.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .rounded(theme::radius::CHIP)
                    .bg(theme::status::ASK_CARD_BG)
                    .border_l(px(2.0))
                    .border_color(theme::status::ASK_CARD_EDGE)
                    .font(font(theme::font::SANS))
                    .text_size(px(10.5))
                    .text_color(theme::status::ASK_CARD_FG)
                    .child(preview.clone()),
            );
        }

        container
    }

    /// Footer 28: real aggregate stats (`N worktrees · disk usage`) plus the real `prune`
    /// action.
    fn render_rail_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Includes error'd entries: `crate::worktrees::WorktreeItem`'s own docs say a real
        // count of what `wt_core::list_worktrees` reported should stay visible problems and
        // all, not silently shrink because some entries failed to read.
        let worktree_count = self.worktrees.len();
        let disk_label = match self.disk_usage {
            Some((bytes, truncated)) => {
                let label = rail::format_bytes(bytes);
                if truncated {
                    format!("{label}+")
                } else {
                    label
                }
            }
            None => "...".to_string(),
        };
        let prunable_count = self.prunable_worktree_paths().len();
        let prune_label = if self.prune_confirm_armed {
            format!("confirm prune ({prunable_count})?")
        } else {
            format!("prune ({prunable_count})")
        };

        div()
            .id("rail-footer")
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .px(px(10.0))
            .h(theme::band::SURFACE_FOOTER)
            .border_t_1()
            .border_color(theme::border::RAIL_INNER)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(if let Some(status) = &self.prune_status {
                        status.clone()
                    } else {
                        format!("{worktree_count} worktrees \u{b7} {disk_label}")
                    }),
            )
            .child(
                div()
                    .id("rail-prune")
                    .cursor_pointer()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(theme::radius::CHIP)
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(if prunable_count > 0 {
                        theme::button::DANGER_FG
                    } else {
                        theme::text::DISABLED
                    })
                    .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                    .child(prune_label)
                    .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                        this.request_prune(cx);
                    })),
            )
    }

    /// A real utility row above the tab strip: "+ shell" / "+ claude" / "+ codex" buttons that
    /// spawn a real session into `active_session_cwd()`, plus a reminder of which worktree
    /// that currently resolves to. Not part of `design_handoff_jerry_ade/Jerry.dc.html` (the
    /// mockup's own rail "+"/tab-strip "+" only ever spawn one default kind - real per-kind
    /// selection lives in the design's Settings › Agents page, which is out of scope here) -
    /// kept as a real, restyled (theme-token, not raw `rgb()`) necessity: without it, this app
    /// would have no way to start a `claude`/`codex` session at all.
    fn render_session_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cwd = self.active_session_cwd();

        let new_session_button = |label: &'static str, kind: SessionKind| {
            div()
                .id(format!("new-session-{}", kind.label()))
                .cursor_pointer()
                .px(px(8.0))
                .py(px(3.0))
                .rounded(theme::radius::CHIP)
                .bg(theme::surface::CHIP_NEUTRAL)
                .font(font(theme::font::MONO))
                .text_size(px(10.5))
                .text_color(theme::text::DIM)
                .hover(|el| {
                    el.bg(theme::surface::ROW_HOVER_ALT)
                        .text_color(theme::text::PRIMARY)
                })
                .child(label)
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.new_session(kind, cx);
                }))
        };

        div()
            .id("session-toolbar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .px(px(12.0))
            .h(px(30.0))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(new_session_button("+ shell", SessionKind::Shell))
            .child(new_session_button("+ claude", SessionKind::Claude))
            .child(new_session_button("+ codex", SessionKind::Codex))
            .child(div().flex_1())
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(format!("new sessions spawn in: {}", cwd.display())),
            )
    }

    /// The tab strip (34) - `design_handoff_jerry_ade/README.md`'s spec: one 14×14 kind chip
    /// per tab (see [`render_tab_chip`]), active/inactive bg/underline/label colours (see
    /// `crate::work_surface::tab_colors`), a real `+` (spawns a new default shell session,
    /// same real action as the rail's own `+`), and the `⌘`/`1…8` keycap hint pinned right.
    fn render_tab_strip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = div()
            .id("tab-strip")
            .flex()
            .flex_none()
            .items_stretch()
            .h(theme::band::TAB_STRIP)
            .bg(theme::surface::TITLE_BAR)
            .border_b_1()
            .border_color(theme::border::ZONE);

        for session in self.sessions.iter() {
            bar = bar.child(self.render_session_tab(session, cx));
        }

        bar = bar.child(
            div()
                .id("tab-strip-new")
                .flex_none()
                .flex()
                .items_center()
                .px(px(11.0))
                .cursor_pointer()
                .font(font(theme::font::MONO))
                .text_size(px(13.0))
                .text_color(theme::text::GHOST)
                .hover(|el| el.text_color(theme::text::MUTED))
                .child("+")
                .on_click(cx.listener(|this, _event: &ClickEvent, _window, cx| {
                    this.new_session(SessionKind::Shell, cx);
                })),
        );

        bar.child(div().flex_1()).child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .px(px(12.0))
                .child(render_keycap_pair("\u{2318}", "1\u{2026}8")),
        )
    }

    /// One tab: a 14×14 kind chip, the real label (the resolved binary name for an agent CLI
    /// tab, or the literal `terminal` for a shell tab - `design_handoff_jerry_ade/README.md`'s
    /// own tab-strip spec), and a real `×` that closes it (`Sessions::close`, tearing down the
    /// real process). Split into a `flex_1` clickable content row plus a `flex_none` 1px
    /// underline bar (rather than a single div with two differently-coloured borders) because
    /// GPUI's `Style::border_color` is one uniform colour for every edge
    /// (`vendor/zed/crates/gpui/src/style.rs`) - it cannot give the right border (always
    /// `theme::border::INNER`) and the bottom "underline" (active/inactive-dependent) two
    /// different colours on the same div.
    fn render_session_tab(&self, session: &Session, cx: &mut Context<Self>) -> impl IntoElement {
        let id = session.id;
        let is_active = self.sessions.active_id() == Some(id);
        let chip_kind = work_surface::tab_chip_kind(session.kind);
        let label = match chip_kind {
            work_surface::TabChipKind::Cli => session.pane.read(cx).program_label(),
            work_surface::TabChipKind::Term => "terminal".to_string(),
        };
        let is_mono = matches!(chip_kind, work_surface::TabChipKind::Cli);
        let colors = work_surface::tab_colors(is_active);

        div()
            .id(("session-tab", id))
            .flex()
            .flex_none()
            .flex_col()
            .border_r_1()
            .border_color(theme::border::INNER)
            .bg(colors.bg)
            .child(
                div()
                    .id(("session-tab-hit", id))
                    .flex_1()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .px(px(13.0))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                        this.select_session(id, cx);
                    }))
                    .child(render_tab_chip(session.kind, is_active))
                    .child(
                        div()
                            .font(font(if is_mono {
                                theme::font::MONO
                            } else {
                                theme::font::SANS
                            }))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(if is_mono { px(11.0) } else { px(11.5) })
                            .text_color(colors.label)
                            .child(label),
                    )
                    .child(
                        div()
                            .id(("close-session-tab", id))
                            .px(px(2.0))
                            .cursor_pointer()
                            .font(font(theme::font::MONO))
                            .text_size(px(11.0))
                            .text_color(theme::text::GHOST)
                            .hover(|el| el.text_color(theme::button::DANGER_FG))
                            .child("\u{d7}")
                            .on_click(cx.listener(
                                move |this, _event: &ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    this.sessions.close(id, cx);
                                    cx.notify();
                                },
                            )),
                    ),
            )
            .child(div().flex_none().w_full().h(px(1.0)).bg(colors.underline))
    }

    /// The session context bar (32) - `design_handoff_jerry_ade/README.md`'s spec: agent
    /// badge/name, a divider, real branch, the real worktree path (the one flexible,
    /// ellipsising child - every other child here is `flex_none` and non-wrapping, matching
    /// the README's own "layout rule that matters" so the bar never wraps when the centre
    /// narrows), a real status pill, and `Merge`/`Archive`.
    fn render_session_context_bar(
        &self,
        session: &Session,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_value = self.session_status(session, cx);
        let (agent_fg, agent_bg) = work_surface::agent_tint(session.kind);
        let agent_initial = work_surface::agent_initial(session.kind);
        // The design's `focus.agent` is a *model* name (`sonnet-4.5`, `gpt-5-codex`) this
        // app's `SessionKind` has no equivalent of (it only tracks which CLI *binary* is
        // running, not which model that CLI is configured to use) - `session.kind.label()`
        // ("Claude"/"Codex"/"Shell") is the closest real, honest substitute, rather than
        // fabricating a model name this app never actually observed.
        let agent_label = session.kind.label();
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.path == session.cwd)
            .and_then(|item| item.branch.clone());
        let worktree_path = session.cwd.display().to_string();
        let id = session.id;

        div()
            .id("session-context-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(theme::band::CONTEXT_BAR)
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .w(px(15.0))
                    .h(px(15.0))
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(agent_bg)
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(8.5))
                    .text_color(agent_fg)
                    .child(agent_initial),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(theme::text::MUTED)
                    .child(agent_label),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(13.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::DIM)
                    .child(branch.unwrap_or_else(|| "(detached)".to_string())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::PATH)
                    .child(worktree_path),
            )
            .child(render_status_pill(status_value))
            .child(render_merge_button())
            .child(self.render_archive_button(id, cx))
    }

    /// The context bar's real `Archive` button - see [`Self::archive_session`]'s docs.
    fn render_archive_button(&self, id: SessionId, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("context-bar-archive", id))
            .flex_none()
            .cursor_pointer()
            .h(px(20.0))
            .px(px(8.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .font(font(theme::font::SANS))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(10.5))
            .text_color(theme::text::FAINT)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .child("Archive")
            .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                this.archive_session(id, cx);
            }))
    }

    /// Surface A/B's shared 27px header - `design_handoff_jerry_ade/README.md`'s Surface A
    /// spec (`claude --resume 3d91e07`-style command, `pid 48213`, right-aligned pty state)
    /// and Surface B spec (`zsh` + worktree path). This app has no saved-session resumability
    /// (no `--resume <sha>` to show - see `crate::work_surface::pty_state_label`'s docs), so
    /// the left label is the real resolved program name alone, never a fabricated resume
    /// argument.
    fn render_pty_header(&self, session: &Session, cx: &mut Context<Self>) -> impl IntoElement {
        let pane = session.pane.read(cx);
        let program_label = pane.program_label();
        let pid = pane.pid();
        let is_running = pane.is_running();
        let exit_code = pane.exit_status().map(|status| status.exit_code());
        let status_value = self.session_status(session, cx);
        let state_label = work_surface::pty_state_label(is_running, status_value, exit_code);

        let header = div()
            .id("pty-header")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(9.0))
            .px(px(12.0))
            .h(theme::band::PTY_HEADER)
            .bg(theme::surface::FOOTER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::DIM)
                    .child(program_label),
            );

        let header = match session.kind {
            SessionKind::Shell => header.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.5))
                    .text_color(theme::text::GHOST)
                    .child(session.cwd.display().to_string()),
            ),
            SessionKind::Claude | SessionKind::Codex => {
                let header = match pid {
                    Some(pid) => header.child(
                        div()
                            .flex_none()
                            .font(font(theme::font::MONO))
                            .text_size(px(10.5))
                            .text_color(theme::text::GHOST)
                            .child(format!("pid {pid}")),
                    ),
                    None => header,
                };
                header.child(div().flex_1())
            }
        };

        header.child(
            div()
                .flex_none()
                .font(font(theme::font::MONO))
                .text_size(px(10.0))
                .text_color(theme::text::HINT)
                .child(state_label),
        )
    }

    /// Surface A/B's shared 28px footer - `design_handoff_jerry_ade/README.md`'s Surface A
    /// spec: the `Jerry` word plus git-level actions appropriate to the session's real status.
    /// See `crate::work_surface::footer_actions`/[`Self::render_footer_action_button`] for
    /// which of those actions are real-and-minimal versus honestly disabled this phase.
    fn render_pty_footer(&self, session: &Session, cx: &mut Context<Self>) -> impl IntoElement {
        let status_value = self.session_status(session, cx);
        let is_running = session.pane.read(cx).is_running();
        let actions = work_surface::footer_actions(status_value);
        let id = session.id;
        let cwd = session.cwd.clone();

        let mut footer = div()
            .id("pty-footer")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .px(px(12.0))
            .h(theme::band::SURFACE_FOOTER)
            .bg(theme::surface::FOOTER)
            .border_t_1()
            .border_color(theme::border::INNER)
            .child(
                div()
                    .flex_none()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_size(px(9.0))
                    .text_color(theme::text::GHOSTER)
                    .child("JERRY"),
            );

        for action in actions {
            let mut enabled = action.implemented;
            // `Resume` (idle status) only means something for a session that has actually
            // exited/never started - a *live*, merely-idle shell has nothing to "resume" (see
            // `crate::work_surface::ActionKind::Respawn`'s docs); real-disable it in that one
            // case rather than letting a click spawn a redundant duplicate session next to a
            // still-running one.
            if action.kind == work_surface::ActionKind::Respawn
                && status_value == Status::Idle
                && is_running
            {
                enabled = false;
            }
            footer = footer.child(self.render_footer_action_button(
                id,
                cwd.clone(),
                action,
                enabled,
                cx,
            ));
        }

        footer.child(div().flex_1())
    }

    /// One footer action button - real (`cursor_pointer`, hover, a real `on_click` dispatch on
    /// `action.kind`) when `enabled`, otherwise the design's own "dimmed, real-disabled"
    /// treatment (no cursor/hover/click at all) - never a button that looks clickable but
    /// silently does nothing.
    fn render_footer_action_button(
        &self,
        id: SessionId,
        cwd: PathBuf,
        action: work_surface::FooterAction,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let colors = work_surface::action_button_colors(action.style);
        let label = action.label;
        let kind = action.kind;

        let mut button = div()
            .id(format!("footer-action-{id}-{label}"))
            .h(px(23.0))
            .px(px(10.0))
            .rounded(theme::radius::BUTTON)
            .flex()
            .items_center()
            .gap(px(7.0))
            .bg(if enabled {
                colors.bg
            } else {
                // A disabled action must never keep its real, full-color fill (that would
                // make an inert button look as, or more, clickable than a real one - exactly
                // what this project's "no fake functionality" rule exists to prevent; a real
                // disabled blue "Resume" was found rendering with the full solid `#243c50`
                // fill next to a real, working "Archive" button). The design itself has no
                // separate disabled-background token - its own disabled precedent
                // (`design_handoff_jerry_ade/README.md`'s "Accept file is always rendered,
                // dimmed (`#454b51` / border `#1f2327`) when there is nothing to accept", and
                // the `Outline`/`Ghost` button styles above, which are already `TRANSPARENT`
                // at full strength) dims only fg/border, never bg - so falling back to
                // `TRANSPARENT` here lets the footer's own background
                // (`theme::surface::FOOTER`) show through and the button visually recede,
                // consistent with that precedent rather than inventing a new muted-fill token.
                work_surface::TRANSPARENT
            })
            .border_1()
            .border_color(if enabled {
                colors.border
            } else {
                theme::border::BUTTON_DISABLED
            })
            .child(
                div()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.0))
                    .text_color(if enabled {
                        colors.fg
                    } else {
                        theme::text::GHOSTER
                    })
                    .child(label),
            );

        if let Some(cap) = action.keycap {
            let (keycap_fg, keycap_border) = if enabled {
                (colors.keycap_fg, colors.keycap_border)
            } else {
                (theme::text::GHOSTER, theme::border::BUTTON_DISABLED)
            };
            button = button.child(render_action_keycap(cap, keycap_fg, keycap_border));
        }

        if enabled {
            button = button
                .cursor_pointer()
                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                .on_click(
                    cx.listener(move |this, _event: &ClickEvent, _window, cx| match kind {
                        work_surface::ActionKind::Interrupt => this.interrupt_session(id, cx),
                        work_surface::ActionKind::OpenTerminal => {
                            this.open_companion_terminal(cwd.clone(), cx)
                        }
                        work_surface::ActionKind::Respawn => this.respawn_session(id, cx),
                        work_surface::ActionKind::Archive => this.archive_session(id, cx),
                        work_surface::ActionKind::Unimplemented => {}
                    }),
                );
        } else {
            button = button.cursor_default();
        }

        button
    }

    /// The centre "work surface" zone. `.min_w_0()` on this method's own root div (the flex
    /// item actually sitting inside the outer three-zone `flex_row`) is the real fix for
    /// the responsive-layout bug this step was asked to fix: see this method's own doc
    /// comment continuation below for the root cause.
    ///
    /// ## Root cause of "typing in the terminal pushes the file tree off-screen"
    ///
    /// This is the classic flexbox "min-width: auto" trap, confirmed against GPUI's real
    /// (Taffy-based) layout engine rather than assumed: a flex item's minimum width before
    /// it's allowed to shrink below its flex-basis defaults to its *content's* intrinsic
    /// (min-content) width, unless something overrides it. `crate::terminal_pane::render_row`
    /// lays out each terminal row as `div().flex().flex_row()` of unwrapped text spans -
    /// `crate::terminal_pane::maybe_resize_pty` sized the grid from the *whole window's*
    /// viewport width (a separate bug, since fixed - see that function's own docs), so a
    /// wide window meant wide rows, and wide unbroken text spans have a large intrinsic
    /// width. Before this fix, this method's own root div - the flex item sitting directly
    /// in the outer `flex_row` of [sidebar, centre, sidebar] in `Render for AdeApp` - had
    /// neither `overflow_hidden()` nor `min_w_0()`, so *its own* automatic minimum width was
    /// still derived from its content (bubbling all the way up from the terminal's widest
    /// row), and it grew to fit that instead of being held to its `flex_1` share - pushing
    /// the fixed-width right sidebar off the visible window.
    ///
    /// The fix is `.min_w_0()` on *this* div specifically (verified real,
    /// `vendor/zed/crates/gpui_macros/src/styles.rs`'s generated box-style suffix, mirroring
    /// CSS `min-width: 0`), confirmed against `vendor/zed/crates/workspace/src/status_bar.rs`'s
    /// own real `.flex_1().min_w_0()` pattern for exactly this situation. Note this is
    /// *not* about `overflow_hidden()` merely "clipping paint but not layout" - GPUI's own
    /// `Overflow::Hidden` doc comment (`vendor/zed/crates/gpui/src/style.rs`) says plainly
    /// that non-`Visible` overflow *does* zero a node's automatic minimum size for
    /// flex/grid layout, the same effect `min_w_0()` has. The reason `overflow_hidden()`
    /// alone didn't already fix this is narrower: that zeroing only applies to the node
    /// it's set on. `TerminalPane::render`'s root div already had `overflow_hidden()` from
    /// step 3 onward, so its own automatic minimum size was already zero - but that node
    /// isn't the one sitting in the outer three-zone `flex_row`; this method's own root div
    /// is, and *it* had `overflow: Visible` (the default) the whole time. `min_w_0()` here
    /// is the fix; the `min_w_0()`/`overflow_hidden()` combination on the inner wrapper
    /// below and on `TerminalPane`'s own root div are cheap, correct, defense-in-depth
    /// (each additionally zeroes its own node's contribution), not load-bearing on their
    /// own - this div is the one that actually mattered.
    fn render_center_pane(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let surface = div()
            .id("work-surface")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme::surface::CENTER)
            .child(self.render_session_toolbar(cx))
            .child(self.render_tab_strip(cx));

        match self.sessions.active() {
            Some(session) => surface
                .child(self.render_session_context_bar(session, cx))
                .child(
                    div()
                        .id("pty-surface")
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .overflow_hidden()
                        .bg(theme::surface::PTY)
                        .child(self.render_pty_header(session, cx))
                        .child(
                            div()
                                .flex_1()
                                .min_h_0()
                                .min_w_0()
                                .overflow_hidden()
                                .child(session.pane.clone().into_any_element()),
                        )
                        .child(self.render_pty_footer(session, cx)),
                ),
            None => surface.child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .items_center()
                    .justify_center()
                    .font(font(theme::font::SANS))
                    .text_size(px(11.5))
                    .text_color(theme::text::FAINT)
                    .child("no sessions open - start one with the buttons above"),
            ),
        }
    }

    fn render_file_tree(&self) -> gpui::AnyElement {
        let mut list = div().flex().flex_col().p_2().size_full();

        list = list.child(
            div()
                .text_xs()
                .text_color(rgb(0x8a8a8a))
                .pb_1()
                .child(self.file_tree_root.display().to_string()),
        );

        if let Some(error) = &self.file_tree_error {
            return list
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xff6b6b))
                        .child(format!("failed to read directory: {error}")),
                )
                .into_any_element();
        }

        if self.file_tree.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .child("(empty directory)"),
            );
        }

        // Only the first `MAX_RENDERED_FILE_ENTRIES` rows are turned into actual elements.
        // `self.file_tree` itself can hold up to `file_tree::MAX_ENTRIES` (5000) real
        // entries - fine as loaded data, but laying out that many `div`s through GPUI's
        // flexbox engine on *every* render (which happens as often as every ~33ms while
        // the terminal pane is streaming output and calling `cx.notify()`) turned out to
        // be a real, measured performance bug during this step's own verification: with a
        // target repo whose tree includes a large nested checkout (`vendor/zed`, ~5000
        // entries on its own), unbounded rendering here starved the foreground executor
        // badly enough that unrelated timers (e.g. the worktree/file-tree load callbacks)
        // were observed firing 10+ seconds late instead of near-instantly. Capping the
        // *rendered* rows (independent of the *loaded* cap) fixes that; a real
        // virtualized list (`uniform_list`, see `vendor/zed/crates/project_panel`) would
        // be the following step for a tree of unbounded size, but is out of scope here.
        let rendered_count = self.file_tree.len().min(MAX_RENDERED_FILE_ENTRIES);
        for entry in &self.file_tree[..rendered_count] {
            let indent = gpui::px(12.0 * entry.depth as f32);
            let icon = if entry.is_dir {
                "\u{1F4C1}"
            } else {
                "\u{1F4C4}"
            };
            list = list.child(
                div()
                    .id(format!("file-{}", entry.path.display()))
                    .flex()
                    .pl(indent)
                    .text_xs()
                    .text_color(rgb(0xcccccc))
                    .child(format!("{icon} {}", entry.name)),
            );
        }

        if self.file_tree.len() > rendered_count {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .pt_1()
                    .child(format!(
                        "... and {} more entries not shown",
                        self.file_tree.len() - rendered_count
                    )),
            );
        }

        list.into_any_element()
    }

    /// The small "Files" / "Diff" toggle at the top of the right sidebar, switching what
    /// [`Self::render_right_sidebar_body`] shows for the currently selected worktree.
    fn render_right_sidebar_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let toggle_button = |label: &'static str, view: RightSidebarView| {
            let is_active = self.right_sidebar_view == view;
            div()
                .id(label)
                .cursor_pointer()
                .flex_1()
                .px_2()
                .py_1()
                .text_xs()
                .text_center()
                .rounded_sm()
                .when(is_active, |el| el.bg(rgb(0x2f5f8f)))
                .when(!is_active, |el| el.hover(|el| el.bg(rgb(0x2a2a2a))))
                .text_color(rgb(0xe0e0e0))
                .child(label)
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.set_right_sidebar_view(view, cx);
                }))
        };

        div()
            .flex()
            .flex_row()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(rgb(0x2a2a2a))
            .child(toggle_button("Files", RightSidebarView::Files))
            .child(toggle_button("Diff", RightSidebarView::Diff))
    }

    /// The right sidebar's content: the real file tree, or the real diff against the
    /// selected worktree's base branch, per [`Self::right_sidebar_view`].
    fn render_right_sidebar_body(&self) -> gpui::AnyElement {
        match self.right_sidebar_view {
            RightSidebarView::Files => self.render_file_tree(),
            RightSidebarView::Diff => self.render_diff(),
        }
    }

    /// Renders [`Self::diff_state`] for [`Self::diff_root`]: a loading/error state, one of
    /// `wt_core::diff::DiffBase`'s explanatory non-diff outcomes (on the default branch, or
    /// no base could be found), or the real diff itself.
    fn render_diff(&self) -> gpui::AnyElement {
        let mut list = div().flex().flex_col().p_2().size_full();

        list = list.child(
            div()
                .text_xs()
                .text_color(rgb(0x8a8a8a))
                .pb_1()
                .child(self.diff_root.display().to_string()),
        );

        match &self.diff_state {
            DiffLoadState::Loading => list
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8a8a8a))
                        .child("computing diff..."),
                )
                .into_any_element(),
            DiffLoadState::Error(err) => list
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xff6b6b))
                        .child(format!("failed to compute diff: {err}")),
                )
                .into_any_element(),
            DiffLoadState::Loaded(DiffBase::NoBaseFound) => list
                .child(div().text_xs().text_color(rgb(0x8a8a8a)).child(
                    "no base branch could be detected for this worktree (no origin/HEAD, \
                     no local main/master, and no fallback branch found)",
                ))
                .into_any_element(),
            DiffLoadState::Loaded(DiffBase::OnDefaultBranch { branch }) => list
                .child(div().text_xs().text_color(rgb(0x8a8a8a)).child(format!(
                    "this worktree is on the default branch ({branch}); nothing to \
                             diff against"
                )))
                .into_any_element(),
            DiffLoadState::Loaded(DiffBase::Diff(diff)) => {
                list = list.child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x8a8a8a))
                        .pb_1()
                        .child(format!(
                            "diff against {} ({})",
                            diff.base_branch,
                            &diff.base_commit[..diff.base_commit.len().min(10)]
                        )),
                );

                if diff.truncated {
                    list = list.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xd4a017))
                            .pb_1()
                            .child("diff output was too large; some files/lines are omitted"),
                    );
                }

                if diff.files.is_empty() {
                    return list
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(0x8a8a8a))
                                .child("no changes"),
                        )
                        .into_any_element();
                }

                let rendered_count = diff.files.len().min(MAX_RENDERED_DIFF_FILES);
                for file in &diff.files[..rendered_count] {
                    list = list.child(self.render_diff_file(file));
                }
                if diff.files.len() > rendered_count {
                    list = list.child(div().text_xs().text_color(rgb(0x8a8a8a)).pt_1().child(
                        format!(
                            "... and {} more changed files not shown",
                            diff.files.len() - rendered_count
                        ),
                    ));
                }

                list.into_any_element()
            }
        }
    }

    /// Renders one changed file: its status/path header, then either a "binary file" note or
    /// its hunks as unified-diff-style, color-coded lines (added/removed/context) - capped by
    /// [`MAX_RENDERED_DIFF_LINES_PER_FILE`] independent of `wt_core::diff`'s own load-time cap
    /// (see that constant's docs).
    fn render_diff_file(&self, file: &DiffFile) -> impl IntoElement {
        let (status_label, status_color) = match file.status {
            FileChangeStatus::Added => ("A", rgb(0x7ee787)),
            FileChangeStatus::Deleted => ("D", rgb(0xffa198)),
            FileChangeStatus::Modified => ("M", rgb(0xd4a017)),
            FileChangeStatus::Renamed => ("R", rgb(0x6ab0f3)),
        };

        let path_text = match &file.old_path {
            Some(old) => format!("{} -> {}", old.display(), file.path.display()),
            None => file.path.display().to_string(),
        };

        let mut container = div()
            .id(format!("diff-file-{}", file.path.display()))
            .flex()
            .flex_col()
            .pb_2()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .items_center()
                    .child(div().text_xs().text_color(status_color).child(status_label))
                    .child(div().text_xs().text_color(rgb(0xe0e0e0)).child(path_text)),
            );

        if file.is_binary {
            return container.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .pl_2()
                    .child("binary file (contents not diffed)"),
            );
        }

        let mut rendered_lines = 0usize;
        let mut hunks_truncated = false;
        'hunks: for hunk in &file.hunks {
            container = container.child(
                div()
                    .text_xs()
                    .font(font(theme::font::MONO))
                    .text_color(rgb(0x6ab0f3))
                    .pl_2()
                    .child(hunk.header.clone()),
            );
            for line in &hunk.lines {
                if rendered_lines >= MAX_RENDERED_DIFF_LINES_PER_FILE {
                    hunks_truncated = true;
                    break 'hunks;
                }
                rendered_lines += 1;

                let (prefix, fg, bg) = match line.kind {
                    DiffLineKind::Added => ("+", rgb(0x7ee787), Some(rgb(0x0f2a1a))),
                    DiffLineKind::Removed => ("-", rgb(0xffa198), Some(rgb(0x2d1214))),
                    DiffLineKind::Context => (" ", rgb(0xb0b0b0), None),
                };
                let mut line_el = div()
                    .text_xs()
                    .font(font(theme::font::MONO))
                    .pl_2()
                    .text_color(fg);
                if let Some(bg) = bg {
                    line_el = line_el.bg(bg);
                }
                container = container.child(line_el.child(format!("{prefix}{}", line.content)));
            }
        }

        if file.truncated || hunks_truncated {
            container = container.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .pl_2()
                    .child("... diff truncated for this file"),
            );
        }

        container
    }
}

/// One keyboard-shortcut keycap, per `design_handoff_jerry_ade/README.md`'s "Keyboard
/// affordances" spec: 15 high, min-width 15, padding 0 4, radius 3, bg `#181c1f`, border 1px
/// `#272c31`, 9.5px/450 mono `#7d848b`.
fn render_keycap(label: &'static str) -> impl IntoElement {
    div()
        .h(theme::band::KEYCAP)
        .min_w(theme::band::KEYCAP)
        .px(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius::CHIP)
        .bg(theme::surface::KEYCAP)
        .border_1()
        .border_color(theme::border::KEYCAP)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(theme::text::DIMMER)
        .child(label)
}

/// A `⌘` + letter keycap pair, e.g. the rail header's `⌘N`.
fn render_keycap_pair(modifier: &'static str, key: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(3.0))
        .child(render_keycap(modifier))
        .child(render_keycap(key))
}

/// One footer-action keycap with the *button's own* tint (`design_handoff_jerry_ade/
/// README.md`'s "Keyboard affordances": "Inside a coloured button the cap goes transparent and
/// borrows the button's tint") - unlike [`render_keycap`] (the rail/tab-strip's always-neutral
/// keycaps), this one's colours vary per `crate::work_surface::ActionStyle` (see
/// `crate::work_surface::action_button_colors`).
fn render_action_keycap(
    label: &'static str,
    fg: gpui::Rgba,
    border: gpui::Rgba,
) -> impl IntoElement {
    div()
        .flex_none()
        .h(theme::band::KEYCAP)
        .min_w(theme::band::KEYCAP)
        .px(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(theme::radius::CHIP)
        .border_1()
        .border_color(border)
        .font(font(theme::font::MONO))
        .text_size(px(9.5))
        .text_color(fg)
        .child(label)
}

/// The tab strip's 14×14 kind chip - a `❯` glyph tinted with the session's real agent colour
/// for agent CLI tabs, or the pane glyph (a 14×4 bar plus a 5×2 prompt mark, per
/// `design_handoff_jerry_ade/README.md`'s tab-strip spec) for terminal tabs. Turns
/// `crate::work_surface::tab_chip_kind`/`tab_chip_colors`'s real, unit-tested mapping into
/// actual GPUI elements - no chip-selection *logic* lives here.
fn render_tab_chip(kind: SessionKind, active: bool) -> gpui::AnyElement {
    let colors = work_surface::tab_chip_colors(kind, active);
    let base = div()
        .flex_none()
        .w(px(14.0))
        .h(px(14.0))
        .rounded(theme::radius::CHIP)
        .bg(colors.bg);

    match work_surface::tab_chip_kind(kind) {
        work_surface::TabChipKind::Cli => base
            .flex()
            .items_center()
            .justify_center()
            .font(font(theme::font::MONO))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_size(px(8.0))
            .text_color(colors.fg)
            .child("\u{276f}")
            .into_any_element(),
        work_surface::TabChipKind::Term => base
            .relative()
            .overflow_hidden()
            .child(
                div()
                    .absolute()
                    .left(px(0.0))
                    .top(px(0.0))
                    .w(px(14.0))
                    .h(px(4.0))
                    .bg(colors.fg),
            )
            .child(
                div()
                    .absolute()
                    .left(px(3.0))
                    .top(px(7.0))
                    .w(px(5.0))
                    .h(px(2.0))
                    .rounded(px(1.0))
                    .bg(colors.fg),
            )
            .into_any_element(),
    }
}

/// The session context bar's real status pill - `design_handoff_jerry_ade/README.md`: "status
/// pill (19 high, radius 3, 5px dot + 10px/500 label in the status colour)".
fn render_status_pill(status: Status) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(19.0))
        .px(px(7.0))
        .rounded(theme::radius::CHIP)
        .bg(status.pill_bg())
        .child(
            div()
                .flex_none()
                .w(px(5.0))
                .h(px(5.0))
                .rounded(px(2.5))
                .bg(status.color()),
        )
        .child(
            div()
                .flex_none()
                .font(font(theme::font::SANS))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_size(px(10.0))
                .text_color(status.color())
                .child(status.label()),
        )
}

/// The context bar's `Merge` action - honestly, visibly disabled. `wt_core`'s only mutating
/// entry points are `add_worktree`/`remove_worktree` (verified: `crates/wt-core/src/lib.rs`) -
/// there is no merge/rebase/fast-forward operation to wire this to yet, and faking one (or
/// reimplementing a real merge flow just for this button) is out of scope for this phase.
/// Rendered with the design's own "dimmed, real-disabled" treatment
/// (`design_handoff_jerry_ade/README.md`'s own precedent for `Accept file`: "dimmed ... when
/// there is nothing to accept ... never a button that looks clickable but silently does
/// nothing") instead of the mockup's default-active outline styling, and deliberately has no
/// `.cursor_pointer()`/`.hover()`/`.on_click()` at all.
fn render_merge_button() -> impl IntoElement {
    div()
        .flex_none()
        .cursor_default()
        .h(px(20.0))
        .px(px(8.0))
        .rounded(theme::radius::BUTTON)
        .border_1()
        .border_color(theme::border::BUTTON_DISABLED)
        .flex()
        .items_center()
        .font(font(theme::font::SANS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_size(px(10.5))
        .text_color(theme::text::GHOSTER)
        .child("Merge")
}

/// One flat-circle window-control button (see `AdeApp::render_window_controls`'s docs for
/// why these are real controls, not decoration) - `on_activate` is called with real
/// `&mut Window`/`&mut App` access so it can invoke a real `Window` control method.
fn window_control_dot(
    id: &'static str,
    on_activate: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .w(px(11.0))
        .h(px(11.0))
        .rounded(px(5.5))
        .bg(theme::text::GUTTER)
        .cursor_pointer()
        .hover(|el| el.bg(theme::text::FAINT))
        .on_click(move |_event, window, cx| {
            cx.stop_propagation();
            on_activate(window, cx);
        })
}

impl AdeApp {
    /// The three flat-circle window controls in the title bar's left cluster (matching
    /// `design_handoff_jerry_ade/Jerry.dc.html`'s three `#3a3f44` dots at the very start of
    /// its title bar - the design doesn't colour-code them the way macOS's traffic lights
    /// are, so this keeps that flat, neutral look while wiring each dot to a real GPUI
    /// window-control method (verified at `vendor/zed/crates/gpui/src/window.rs`:
    /// `remove_window` (`:2016`, used directly by `vendor/zed/crates/gpui/examples/
    /// on_window_close_quit.rs:19`), `minimize_window` (`:5520`), `zoom_window` (`:2489`,
    /// toggles maximize/restore) - the same three calls
    /// `vendor/zed/crates/platform_title_bar/src/platforms/platform_linux.rs`'s own
    /// `WindowControl::on_click` makes. Left-to-right order (close, minimize, maximize)
    /// mirrors that same three-flat-dot visual grouping's most common real-world reading
    /// (macOS traffic lights); this design deliberately doesn't colour-code them, so there
    /// is no ordering hint from the mockup itself - a judgment call, not a spec value.
    ///
    /// The wrapping row stops left-click propagation on mouse-down, mirroring
    /// `vendor/zed/crates/platform_title_bar/src/platforms/platform_linux.rs`'s
    /// `LinuxWindowControls` (`.on_mouse_down(MouseButton::Left, |_, _, cx|
    /// cx.stop_propagation())`), so pressing a dot can never also arm
    /// `Self::render_title_bar`'s window-move drag.
    fn render_window_controls(&self) -> impl IntoElement {
        div()
            .id("window-controls")
            .flex()
            .gap(px(8.0))
            .pl(px(2.0))
            .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                cx.stop_propagation();
            })
            .child(window_control_dot("title-bar-close", |window, _cx| {
                window.remove_window();
            }))
            .child(window_control_dot("title-bar-minimize", |window, _cx| {
                window.minimize_window();
            }))
            .child(window_control_dot("title-bar-maximize", |window, _cx| {
                window.zoom_window();
            }))
    }

    /// The 38px title-bar band (`design_handoff_jerry_ade/README.md`'s Layout table: height
    /// 38, bg `#101214`, bottom border `#1e2225`) - real window content, not OS chrome (see
    /// this step's task docs: the README's "the real app gets OS window chrome" refers to
    /// the *outer* window frame, and this band draws itself regardless of that). It carries
    /// [`Self::render_window_controls`], a divider, and the real project name/branch (the
    /// repository directory name and the main worktree's real detected branch, once
    /// `Self::load_worktrees` has resolved - never a placeholder string).
    ///
    /// ## Dragging the window
    ///
    /// GPUI has no single "make this element drag the window" method; the real pattern
    /// (verified against `vendor/zed/crates/platform_title_bar/src/platform_title_bar.rs`'s
    /// own title bar, which faces the identical "Wayland/X11 have no native draggable
    /// titlebar for a client-side-decorated window" problem) is: mark the area with
    /// `.window_control_area(WindowControlArea::Drag)` (`vendor/zed/crates/gpui/src/
    /// elements/div.rs:1167`, a hit-test hint the compositor consults for double-click/
    /// right-click gestures - `vendor/zed/crates/gpui/src/window.rs:1747`'s
    /// `on_hit_test_window_control`), then drive the actual move from ordinary mouse
    /// events: arm [`Self::title_bar_move_armed`] on left mouse-down, and on the next
    /// mouse-move (still armed) call the real `Window::start_window_move`
    /// (`window.rs:2502` - "tells the compositor to take control of window movement
    /// (Wayland and X11)") and disarm. `on_mouse_up`/`on_mouse_down_out` also disarm, so a
    /// click that never moves (e.g. clicking to focus the window) never starts a move.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let project_name = self
            .repo_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.repo_path.display().to_string());
        let branch = self
            .worktrees
            .iter()
            .find(|item| item.is_main)
            .and_then(|item| item.branch.clone());

        div()
            .id("title-bar")
            .window_control_area(WindowControlArea::Drag)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(14.0))
            .px(px(12.0))
            .w_full()
            .h(theme::band::TITLE_BAR)
            .bg(theme::surface::TITLE_BAR)
            .border_b_1()
            .border_color(theme::border::ZONE)
            .on_mouse_down_out(cx.listener(|this, _event, _window, _cx| {
                this.title_bar_move_armed = false;
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.title_bar_move_armed = false;
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _event, _window, _cx| {
                    this.title_bar_move_armed = true;
                }),
            )
            .on_mouse_move(cx.listener(|this, _event, window, _cx| {
                if this.title_bar_move_armed {
                    this.title_bar_move_armed = false;
                    window.start_window_move();
                }
            }))
            .child(self.render_window_controls())
            .child(div().w(px(1.0)).h(px(16.0)).bg(theme::border::DIVIDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(12.0))
                            .text_color(theme::text::STRONG)
                            .child(project_name),
                    )
                    .when_some(branch, |el, branch| {
                        el.child(
                            div()
                                .font(font(theme::font::MONO))
                                .text_size(px(11.0))
                                .text_color(theme::text::FAINTER)
                                .child(branch),
                        )
                    }),
            )
            .child(div().flex_1())
    }

    /// The 26px status bar (`design_handoff_jerry_ade/README.md`'s Layout table: height 26,
    /// bg `#101214`, top border `#1e2225`). The mockup's own content here (`↑2 ↓0` ahead/
    /// behind counts, a `{{ statusLine }}` template placeholder, and `⌘K`/`⌘⇧K` command-
    /// palette hints) either needs git plumbing this phase doesn't build or a command
    /// palette that doesn't exist yet (phase A's task explicitly leaves the palette for a
    /// later phase) - rendering those would be exactly the "component bound to nothing"
    /// this project's constraints forbid. This phase's status bar instead shows only real,
    /// already-available data: the repository root path and how many real worktrees
    /// `Self::load_worktrees` found.
    fn render_status_bar(&self) -> impl IntoElement {
        let worktree_count = self.worktrees.len();
        let label = match worktree_count {
            1 => "1 worktree".to_string(),
            n => format!("{n} worktrees"),
        };

        div()
            .id("status-bar")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(12.0))
            .px(px(12.0))
            .w_full()
            .h(theme::band::STATUS_BAR)
            .bg(theme::surface::TITLE_BAR)
            .border_t_1()
            .border_color(theme::border::ZONE)
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(self.repo_path.display().to_string()),
            )
            .child(div().flex_1())
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(px(10.0))
                    .text_color(theme::text::GHOST)
                    .child(label),
            )
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
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .id("body")
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("worktree-sidebar")
                            .flex_none()
                            .w(theme::zone::RAIL_WIDTH)
                            .h_full()
                            .bg(theme::surface::RAIL)
                            .border_r_1()
                            .border_color(theme::border::ZONE)
                            .child(self.render_rail(cx)),
                    )
                    .child(self.render_center_pane(cx))
                    .child(
                        div()
                            .id("file-tree-sidebar")
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(theme::zone::PANEL_WIDTH)
                            .h_full()
                            .bg(theme::surface::RAIL)
                            .border_l_1()
                            .border_color(theme::border::ZONE)
                            .child(self.render_right_sidebar_toggle(cx))
                            .child(
                                div()
                                    .id("right-sidebar-body")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .child(self.render_right_sidebar_body()),
                            ),
                    ),
            )
            .child(self.render_status_bar())
    }
}
