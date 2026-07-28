//! The top-level three-pane window: a left worktree sidebar, a center terminal pane, and a
//! right file tree, composed as GPUI entities.
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

use std::path::PathBuf;

use gpui::{div, prelude::*, rgb, ClickEvent, Context, Entity, Task, Window};

use crate::file_tree::{self, FileTreeEntry};
use crate::terminal_pane::TerminalPane;
use crate::worktrees::{self, WorktreeItem};

const SIDEBAR_WIDTH: gpui::Pixels = gpui::px(240.0);

/// See the comment at its use site in `render_file_tree` for why this exists.
const MAX_RENDERED_FILE_ENTRIES: usize = 500;

pub struct AdeApp {
    repo_path: PathBuf,
    worktrees: Vec<WorktreeItem>,
    worktrees_error: Option<String>,
    selected: Option<usize>,
    terminal: Entity<TerminalPane>,
    file_tree: Vec<FileTreeEntry>,
    file_tree_root: PathBuf,
    file_tree_error: Option<String>,
    _load_worktrees_task: Option<Task<()>>,
    _load_file_tree_task: Option<Task<()>>,
}

impl AdeApp {
    pub fn new(repo_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let terminal = cx.new(|cx| TerminalPane::new(repo_path.clone(), cx));

        let mut this = Self {
            file_tree_root: repo_path.clone(),
            repo_path: repo_path.clone(),
            worktrees: Vec::new(),
            worktrees_error: None,
            selected: None,
            terminal,
            file_tree: Vec::new(),
            file_tree_error: None,
            _load_worktrees_task: None,
            _load_file_tree_task: None,
        };
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
        self.terminal
            .update(cx, |terminal, cx| terminal.respawn(path.clone(), cx));
        self.load_file_tree(path, cx);
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .h_full()
                    .child(self.terminal.clone()),
            )
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
