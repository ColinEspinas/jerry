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

use gpui::{div, prelude::*, rgb, ClickEvent, Context, Task, Window};

use crate::file_tree::{self, FileTreeEntry};
use crate::sessions::{SessionKind, Sessions};
use crate::worktrees::{self, WorktreeItem};

const SIDEBAR_WIDTH: gpui::Pixels = gpui::px(240.0);

/// See the comment at its use site in `render_file_tree` for why this exists.
const MAX_RENDERED_FILE_ENTRIES: usize = 500;

pub struct AdeApp {
    repo_path: PathBuf,
    worktrees: Vec<WorktreeItem>,
    worktrees_error: Option<String>,
    selected: Option<usize>,
    sessions: Sessions,
    file_tree: Vec<FileTreeEntry>,
    file_tree_root: PathBuf,
    file_tree_error: Option<String>,
    _load_worktrees_task: Option<Task<()>>,
    _load_file_tree_task: Option<Task<()>>,
}

impl AdeApp {
    pub fn new(repo_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            file_tree_root: repo_path.clone(),
            repo_path: repo_path.clone(),
            worktrees: Vec::new(),
            worktrees_error: None,
            selected: None,
            sessions: Sessions::new(),
            file_tree: Vec::new(),
            file_tree_error: None,
            _load_worktrees_task: None,
            _load_file_tree_task: None,
        };
        // A fresh window shouldn't open with zero tabs and no way to see anything running -
        // start with one real shell in the repo root, exactly like step 3's single terminal
        // did, except now it's a tab like any other rather than the only pane that can
        // exist.
        this.sessions
            .spawn(SessionKind::Shell, repo_path.clone(), cx);
        this.load_worktrees(cx);
        this.load_file_tree(repo_path, cx);
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
        self.load_file_tree(path, cx);
        cx.notify();
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
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .child(self.render_session_toolbar(cx))
            .child(self.render_tab_bar(cx))
            .child(div().flex().flex_col().flex_1().min_h_0().child(body))
    }

    fn render_file_tree(&self) -> impl IntoElement {
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
}

impl Render for AdeApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .size_full()
            .bg(rgb(0x181818))
            .child(
                div()
                    .id("worktree-sidebar")
                    .flex_none()
                    .w(SIDEBAR_WIDTH)
                    .h_full()
                    .overflow_y_scroll()
                    .border_r_1()
                    .border_color(rgb(0x2a2a2a))
                    .child(self.render_worktree_sidebar(cx)),
            )
            .child(self.render_center_pane(cx))
            .child(
                div()
                    .id("file-tree-sidebar")
                    .flex_none()
                    .w(SIDEBAR_WIDTH)
                    .h_full()
                    .overflow_y_scroll()
                    .border_l_1()
                    .border_color(rgb(0x2a2a2a))
                    .child(self.render_file_tree()),
            )
    }
}
