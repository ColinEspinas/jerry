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

use std::path::PathBuf;

use gpui::{
    div, font, prelude::*, px, rgb, App, ClickEvent, Context, MouseButton, Task, Window,
    WindowControlArea,
};
use wt_core::diff::{DiffBase, DiffFile, DiffLineKind, FileChangeStatus};

use crate::file_tree::{self, FileTreeEntry};
use crate::sessions::{SessionKind, Sessions};
use crate::theme;
use crate::worktrees::{self, WorktreeItem};

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
    _load_worktrees_task: Option<Task<()>>,
    _load_file_tree_task: Option<Task<()>>,
    _load_diff_task: Option<Task<()>>,
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
            _load_worktrees_task: None,
            _load_file_tree_task: None,
            _load_diff_task: None,
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
                cx.notify();
            });
        });
        self._load_worktrees_task = Some(task);
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
        cx.notify();
    }

    fn render_worktree_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap_1().p_2().size_full();

        if let Some(error) = &self.worktrees_error {
            return list
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0xff6b6b))
                        .child(format!("failed to list worktrees: {error}")),
                )
                .into_any_element();
        }

        if self.worktrees.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(rgb(0x8a8a8a))
                    .child("no worktrees found"),
            );
        }

        for (index, item) in self.worktrees.iter().enumerate() {
            let is_selected = self.selected == Some(index);
            let mut row = div()
                .id(format!("worktree-{index}"))
                .flex()
                .flex_col()
                .px_2()
                .py_1()
                .rounded_sm()
                .text_xs()
                .when(is_selected, |row| row.bg(rgb(0x2f5f8f)))
                .when(!is_selected, |row| row.hover(|row| row.bg(rgb(0x2a2a2a))));

            if item.error.is_none() {
                row = row.cursor_pointer().on_click(cx.listener(
                    move |this, _event: &ClickEvent, _window, cx| {
                        this.select_worktree(index, cx);
                    },
                ));
            }

            let label_color = if item.error.is_some() {
                rgb(0xff6b6b)
            } else {
                rgb(0xe0e0e0)
            };

            row = row.child(div().text_color(label_color).child(item.label.clone()));

            if item.is_main {
                row = row.child(div().text_color(rgb(0x8a8a8a)).child("main worktree"));
            }
            if item.is_locked {
                row = row.child(div().text_color(rgb(0xd4a017)).child("locked"));
            }
            if let Some(error) = &item.error {
                row = row.child(div().text_color(rgb(0xff6b6b)).child(error.clone()));
            }

            list = list.child(row);
        }

        list.into_any_element()
    }

    /// The toolbar above the tab bar: "New Shell" / "New Claude Session" buttons that spawn
    /// a real session into `active_session_cwd()`, plus a reminder of which worktree that
    /// currently resolves to.
    fn render_session_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cwd = self.active_session_cwd();

        let new_session_button = |label: &'static str, kind: SessionKind| {
            div()
                .id(format!("new-session-{}", kind.label()))
                .cursor_pointer()
                .px_2()
                .py_1()
                .rounded_sm()
                .text_xs()
                .bg(rgb(0x2a2a2a))
                .hover(|el| el.bg(rgb(0x3a3a3a)))
                .text_color(rgb(0xe0e0e0))
                .child(label)
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.new_session(kind, cx);
                }))
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(rgb(0x2a2a2a))
            .child(new_session_button("+ New Shell", SessionKind::Shell))
            .child(new_session_button(
                "+ New Claude Session",
                SessionKind::Claude,
            ))
            .child(new_session_button(
                "+ New Codex Session",
                SessionKind::Codex,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(0x6a6a6a))
                    .child(format!("new sessions spawn in: {}", cwd.display())),
            )
    }

    /// The tab strip: one entry per open session, click to activate, click the "x" to close
    /// (tearing down its real process - see `Sessions::close`'s docs).
    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_id = self.sessions.active_id();
        let mut bar = div()
            .flex()
            .flex_row()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(rgb(0x2a2a2a));

        for session in self.sessions.iter() {
            let id = session.id;
            let is_active = active_id == Some(id);
            let title = match session.cwd.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => session.cwd.display().to_string(),
            };

            let tab = div()
                .id(("session-tab", id))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded_sm()
                .text_xs()
                .cursor_pointer()
                .when(is_active, |tab| tab.bg(rgb(0x2f5f8f)))
                .when(!is_active, |tab| tab.hover(|tab| tab.bg(rgb(0x2a2a2a))))
                .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                    this.sessions.set_active(id);
                    cx.notify();
                }))
                .child(div().text_color(rgb(0xe0e0e0)).child(format!(
                    "{}: {}",
                    session.kind.label(),
                    title
                )))
                .child(
                    div()
                        .id(("close-session-tab", id))
                        .px_1()
                        .rounded_sm()
                        .text_color(rgb(0x9a9a9a))
                        .hover(|el| el.bg(rgb(0x4a2a2a)).text_color(rgb(0xff6b6b)))
                        .child("x")
                        .on_click(cx.listener(move |this, _event: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.sessions.close(id, cx);
                            cx.notify();
                        })),
                );

            bar = bar.child(tab);
        }

        bar
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
        let body: gpui::AnyElement = match self.sessions.active() {
            Some(session) => session.pane.clone().into_any_element(),
            None => div()
                .flex()
                .size_full()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(rgb(0x6a6a6a))
                .child("no sessions open - start one with the buttons above")
                .into_any_element(),
        };

        div()
            .id("work-surface")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(theme::surface::CENTER)
            .child(self.render_session_toolbar(cx))
            .child(self.render_tab_bar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(body),
            )
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
                            .overflow_y_scroll()
                            .bg(theme::surface::RAIL)
                            .border_r_1()
                            .border_color(theme::border::ZONE)
                            .child(self.render_worktree_sidebar(cx)),
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
