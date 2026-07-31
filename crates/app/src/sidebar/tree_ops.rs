//! The file tree's real write operations (GitHub issue #19): the context menu's state, the
//! inline name editors behind New File / New Folder / Rename, the cut/copy/paste clipboard, and
//! the confirmed delete.
//!
//! Split the way every feature folder in this crate is split: the pure decisions live in
//! [`crate::sidebar::context_menu`] (which rows a target offers, where the popover fits) and
//! [`crate::sidebar::file_ops`] (name validation, collision-free naming, the real `std::fs`
//! calls), and this file is the `impl AdeApp` glue that sequences them and repairs the app's own
//! state afterwards. [`crate::sidebar::render`] only draws what these two decide.
//!
//! ## Keybinding scoping (the bug class this project keeps re-finding)
//!
//! `Ctrl+C`/`Ctrl+X`/`Ctrl+V` are the most dangerous keystrokes this app could bind:
//! `crate::terminal::pane::keystroke_to_bytes` maps an unmodified `Ctrl+<letter>` to the control
//! byte a focused shell expects, and `Ctrl+C` in particular is SIGINT - a version of this that
//! swallowed it would make it impossible to interrupt a running agent CLI. Two independent
//! things stop that here, and it is worth being precise about which one does what, because a
//! first draft of these docs got it wrong and this project's own revert-verification caught it:
//!
//! 1. **The `key_context` predicate**, `Some("file-tree && !tree-editing && !tree-delete-confirm")` (see
//!    `crate::default_key_bindings`). `Window::dispatch_key_event` resolves bindings against the
//!    context stack of the *focused node's own dispatch path*, before any listener is consulted
//!    (`vendor/zed/crates/gpui/src/window.rs`'s `dispatch_key` call). A focused terminal pane is
//!    not inside the tree, so `file-tree` is not on that stack and the action is never produced
//!    at all.
//! 2. **Where the handlers are registered.** `.on_action` for all five tree actions lives on
//!    `crate::sidebar::render::AdeApp::file_tree_shell`'s node - the tree's own container - and
//!    nowhere else. (That node carries seven `on_action` listeners in total: these five, plus the
//!    `TextUndo`/`TextRedo` pair the inline name editor gained when GitHub issue #17's per-widget
//!    text undo merged in. The reasoning below is unchanged by those two - they are scoped
//!    `Some("text-input")`, which the tree only emits while an editor is open.)
//!    A listener found in the dispatch path sets `cx.propagate_event = false`
//!    before running ("Actions stop propagation by default during the bubble phase", same file),
//!    so a listener that *is* found genuinely swallows the keystroke; one that isn't leaves
//!    `propagate_event` true and `finish_dispatch_key_event` still delivers the key to the
//!    focused pane's own `on_key_down`.
//!
//! These two are **independent** - either alone protects a focused terminal, which was confirmed
//! by deliberately breaking each in turn and re-running the tests. An earlier draft of these docs
//! claimed point 2 was the only real protection and that point 1 "isn't doing the
//! terminal-protection work"; that was wrong, and it mattered, because it would have justified a
//! later refactor dropping the `file-tree` half as dead weight.
//!
//! The `!tree-editing` half is a different matter: it has no redundant partner, and it is the one
//! whose absence is directly reproducible. While an inline name editor is open the tree *is* the
//! focused node, so both mechanisms line up in its favour - the listener runs, propagation stops,
//! and the keystroke the user was typing into the name field is genuinely swallowed
//! (`Shift+F10` reopens the context menu on top of the editor; `Ctrl+V` pastes a *file* into the
//! tree). It is the same shape as Revision R8.5b's `"file-editor && !completions"` fix, and it is
//! revert-verified by
//! `tree_ops_regression_tests::shift_f10_while_an_inline_editor_is_open_neither_opens_a_menu_nor_disturbs_the_name`,
//! which fails against a bare `Some("file-tree")`. `!tree-delete-confirm` is the same mechanism
//! again, for the modal delete confirmation.

use super::*;
use crate::sidebar::context_menu::{ContextTarget, MenuAction};
use crate::sidebar::file_ops::{self, DeleteMechanism};
use crate::text_history;
use gpui::{ClipboardItem, KeyDownEvent, Window};
use std::path::Component;
use std::time::Instant;

/// An open context menu: what it targets, and the already-clamped window-space origin its
/// popover paints at. The origin is resolved once, at open time, from the real click position
/// and the real `Window::bounds()` (see [`AdeApp::open_tree_context_menu`]) rather than
/// recomputed per frame - a menu that moved while it was open would be its own bug.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TreeContextMenu {
    pub(crate) target: ContextTarget,
    pub(crate) origin_x: f32,
    pub(crate) origin_y: f32,
}

/// Which of the three inline editors is open. All three share one text field, one validation
/// path, and one Enter/Escape handler - they only differ in what committing them does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InlineEditKind {
    NewFile { parent: PathBuf },
    NewFolder { parent: PathBuf },
    Rename { path: PathBuf, is_dir: bool },
}

impl InlineEditKind {
    /// The row this editor is drawn against: the folder a new entry lands in, or the entry being
    /// renamed. [`crate::sidebar::render::AdeApp::render_file_tree`] uses this to place the
    /// editor at the right spot in the list.
    pub(crate) fn anchor(&self) -> &Path {
        match self {
            InlineEditKind::NewFile { parent } | InlineEditKind::NewFolder { parent } => parent,
            InlineEditKind::Rename { path, .. } => path,
        }
    }

    pub(crate) fn title(&self) -> &'static str {
        match self {
            InlineEditKind::NewFile { .. } => "New file",
            InlineEditKind::NewFolder { .. } => "New folder",
            InlineEditKind::Rename { .. } => "Rename",
        }
    }
}

/// An in-progress inline name editor.
///
/// **This lives on `AdeApp`, never inside `AdeApp::file_tree`, and that is the whole of issue
/// #19 §4's "a watcher refresh must not clobber an in-progress editor" requirement.** The file
/// tree is a `Vec<FileTreeEntry>` that `AdeApp::load_file_tree`'s background walk *replaces
/// wholesale* every time it completes - which is exactly what happens when an agent CLI creates
/// or deletes a file mid-session and something triggers a re-walk. Any editor state stored in
/// that vector, or keyed by an index into it, would be silently destroyed by that replacement,
/// mid-keystroke. Keeping it here means the walk can replace every row without the typed text
/// ever being at risk, and the renderer re-locates the editor's anchor row by *path* on each
/// frame (falling back to the top of the list if the anchor has genuinely gone) rather than by a
/// position that a re-walk can invalidate. This is the same discipline issue #18 applied to fold
/// state, which is likewise held outside the walked tree and re-derived against it.
// Not `Eq`: `text_history::TextField` holds a recorded history whose `Instant` timestamps have no
// meaningful total equality, so it is deliberately `PartialEq` only.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TreeInlineEdit {
    pub(crate) kind: InlineEditKind,
    /// The name typed so far - append/backspace only, the same field shape *and now the same real
    /// undo history* (`crate::text_history::TextField`, GitHub issue #17) that
    /// `crate::root::new_file`'s prompt and the rail's filter row use. This editor was built in
    /// parallel with issue #17 and originally held a bare `String`; making it a `TextField` when
    /// the two branches merged is what gives `Ctrl+Z` while typing a name a real, per-widget
    /// meaning instead of letting it fall through to the *worktree* history - see
    /// `crate::sidebar::AdeApp::file_tree_shell`'s `"text-input"` context word.
    pub(crate) name: text_history::TextField,
    /// The real rejection message shown under the field (issue #19 §2: "invalid names are
    /// rejected with a hint"), cleared on the next keystroke.
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClipboardMode {
    Copy,
    Cut,
}

/// The tree's own cut/copy buffer. Deliberately *not* the system clipboard: this holds a real
/// filesystem entry to be moved or copied, which is a different thing from the text
/// `Copy Path` writes to the system clipboard, and round-tripping a path through text would make
/// "paste" mean something different depending on what some other application had copied last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeClipboard {
    pub(crate) path: PathBuf,
    pub(crate) mode: ClipboardMode,
}

/// A delete that has been *requested* but not yet confirmed. Holding the resolved
/// [`DeleteMechanism`] here - rather than resolving it when the confirmation is accepted - is
/// what makes the confirmation copy honest: the words the user agreed to ("Move to Trash" vs
/// "Delete permanently") and the command that then runs come from the same value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingTreeDelete {
    pub(crate) path: PathBuf,
    pub(crate) is_dir: bool,
    pub(crate) mechanism: DeleteMechanism,
}

impl PendingTreeDelete {
    /// The confirm button's label - the exact promise being made.
    pub(crate) fn confirm_label(&self) -> &'static str {
        match self.mechanism {
            DeleteMechanism::Trash { .. } => "Move to Trash",
            DeleteMechanism::Permanent => "Delete permanently",
        }
    }

    /// The sentence above the buttons. The permanent branch says so in as many words, and names
    /// *why* there is no trash, rather than implying the app chose not to use one.
    pub(crate) fn explanation(&self) -> String {
        let what = if self.is_dir {
            "this folder and everything in it"
        } else {
            "this file"
        };
        match self.mechanism {
            DeleteMechanism::Trash { .. } => {
                format!("Move {what} to the system trash (restorable from your file manager).")
            }
            DeleteMechanism::Permanent => format!(
                "Permanently delete {what}. No OS trash command is available here, so this \
                 cannot be undone from inside or outside this app."
            ),
        }
    }
}

/// `path` with a leading `old` replaced by `new` - `None` when `path` is unrelated to `old`.
/// Used to carry every path-keyed piece of app state across a rename, including the whole
/// subtree beneath a renamed *directory*.
pub(crate) fn remap_path(path: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    if path == old {
        return Some(new.to_path_buf());
    }
    path.strip_prefix(old).ok().map(|rest| new.join(rest))
}

impl AdeApp {
    /// Moves keyboard focus onto the tree, so its `Ctrl+C`/`Ctrl+X`/`Ctrl+V`/`F2`/`Shift+F10`
    /// bindings - all scoped to the `"file-tree"` context - can match at all.
    ///
    /// Called from a right-click on any row or on the empty area, and from a left-click on a
    /// *folder* row. Deliberately **not** from a left-click on a *file* row: that path
    /// (`Self::open_file_view`) opens the file and moves focus to the code surface, which is
    /// what the user asked for, and stealing it back would break typing in the editor they just
    /// opened. The honest consequence is that `F2`/`Shift+F10` on a file need a right-click (or a
    /// folder click) first rather than a plain left-click - which is also where the issue's own
    /// keyboard requirement stops: there are no up/down bindings to *move* the selection within
    /// the tree, and inventing a focus-stealing left-click to paper over that would be worse than
    /// the gap.
    pub(in crate::sidebar) fn focus_file_tree(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.tree_focus_handle, cx);
    }

    /// Opens the context menu for `target` at a real click position, clamped so the whole
    /// popover stays inside the window (`context_menu::clamp_menu_origin`).
    ///
    /// Also focuses the tree and selects the targeted row: a right-click is a real selection
    /// gesture, and `Shift+F10` afterwards has to have something to target.
    pub(in crate::sidebar) fn open_tree_context_menu(
        &mut self,
        target: ContextTarget,
        click_x: f32,
        click_y: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A right-click while a name is being typed would otherwise leave the editor open
        // underneath a menu whose actions all act on a different path.
        if self.tree_inline_edit.is_some() {
            self.cancel_tree_inline_edit(window, cx);
        }
        let rows = context_menu::menu_items(&target, self.tree_clipboard.is_some()).len();
        let viewport = window.bounds().size;
        let (origin_x, origin_y) = context_menu::clamp_menu_origin(
            click_x,
            click_y,
            context_menu::MENU_WIDTH,
            context_menu::menu_height(rows),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        self.selected_tree_path = target.path().map(Path::to_path_buf);
        self.tree_context_menu = Some(TreeContextMenu {
            target,
            origin_x,
            origin_y,
        });
        self.tree_op_error = None;
        self.focus_file_tree(window, cx);
        cx.notify();
    }

    /// `Shift+F10`'s handler: opens the menu for the currently selected row, or for the empty
    /// area when nothing in this tree is selected.
    ///
    /// The origin is the top-left of the sidebar's own painted bounds plus a small offset rather
    /// than a mouse position - there is no cursor involved in a keyboard-opened menu, and
    /// pinning it to the last mouse position would put it somewhere the keyboard user never
    /// looked. Still routed through the same clamp, so it can't escape the window either.
    pub(in crate::sidebar) fn open_tree_context_menu_from_keyboard(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match self.selected_tree_path.clone() {
            Some(path) if path.starts_with(&self.file_tree_root) => {
                let is_dir = self
                    .file_tree
                    .iter()
                    .find(|entry| entry.path == path)
                    .map(|entry| entry.is_dir)
                    .unwrap_or_else(|| path.is_dir());
                if is_dir {
                    ContextTarget::Folder(path)
                } else {
                    ContextTarget::File(path)
                }
            }
            _ => ContextTarget::Empty,
        };
        let bounds = self.file_tree_bounds;
        let x = f32::from(bounds.origin.x) + 12.0;
        let y = f32::from(bounds.origin.y) + 12.0;
        self.open_tree_context_menu(target, x, y, window, cx);
    }

    pub(in crate::sidebar) fn close_tree_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.tree_context_menu.take().is_some() {
            cx.notify();
        }
    }

    /// Runs one menu row. Every branch closes the menu first, so no action can ever run against
    /// a menu that is still on screen claiming a different state.
    pub(in crate::sidebar) fn run_tree_menu_action(
        &mut self,
        action: MenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.tree_context_menu.take() else {
            return;
        };
        let root = self.file_tree_root.clone();
        let destination = menu.target.destination_dir(&root).to_path_buf();
        let target_path = menu.target.path().map(Path::to_path_buf);
        self.tree_op_error = None;

        match action {
            MenuAction::Open => {
                if let Some(path) = target_path {
                    self.open_file_view(path, window, cx);
                }
            }
            MenuAction::NewFile => self.start_tree_new_entry(destination, false, window, cx),
            MenuAction::NewFolder => self.start_tree_new_entry(destination, true, window, cx),
            MenuAction::Rename => {
                if let Some(path) = target_path {
                    let is_dir = matches!(menu.target, ContextTarget::Folder(_));
                    self.start_tree_rename(path, is_dir, window, cx);
                }
            }
            MenuAction::Duplicate => {
                if let Some(path) = target_path {
                    self.duplicate_tree_entry(&path, cx);
                }
            }
            MenuAction::Cut => {
                if let Some(path) = target_path {
                    self.set_tree_clipboard(path, ClipboardMode::Cut, cx);
                }
            }
            MenuAction::Copy => {
                if let Some(path) = target_path {
                    self.set_tree_clipboard(path, ClipboardMode::Copy, cx);
                }
            }
            MenuAction::Paste => self.paste_into_dir(&destination, cx),
            MenuAction::CopyPath => {
                if let Some(path) = target_path {
                    self.copy_path_to_system_clipboard(&path, false, cx);
                }
            }
            MenuAction::CopyRelativePath => {
                if let Some(path) = target_path {
                    self.copy_path_to_system_clipboard(&path, true, cx);
                }
            }
            MenuAction::CollapseSubtree => {
                if let Some(path) = target_path {
                    self.collapse_subtree(&path, cx);
                }
            }
            // Genuinely the same method issue #18 built and the Files header's own "collapse
            // all" button calls - the live set, this worktree's persisted entry, and the queued
            // write all reset in one step. Not a second, parallel mechanism.
            MenuAction::CollapseAll => self.collapse_all_dirs(cx),
            MenuAction::Delete => {
                if let Some(path) = target_path {
                    let is_dir = matches!(menu.target, ContextTarget::Folder(_));
                    self.request_tree_delete(path, is_dir, cx);
                }
            }
            MenuAction::Reveal => {
                if let Some(path) = target_path {
                    self.reveal_in_file_manager(&path, cx);
                }
            }
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- inline name editors

    /// Opens the inline "New file"/"New folder" editor anchored to `parent`, expanding `parent`
    /// first so the editor row is genuinely visible (the tree opens collapsed - issue #18 §1 -
    /// so an editor inside an unexpanded folder would be an editor nobody can see).
    pub(in crate::sidebar) fn start_tree_new_entry(
        &mut self,
        parent: PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if parent != self.file_tree_root {
            self.set_dir_expanded(parent.clone(), true, cx);
        }
        self.tree_inline_edit = Some(TreeInlineEdit {
            kind: if is_dir {
                InlineEditKind::NewFolder { parent }
            } else {
                InlineEditKind::NewFile { parent }
            },
            name: text_history::TextField::new(),
            error: None,
        });
        self.focus_file_tree(window, cx);
        cx.notify();
    }

    /// Opens the inline rename editor on `path`, pre-filled with its current name (issue #19
    /// §2's `F2`).
    pub(in crate::sidebar) fn start_tree_rename(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if path == self.file_tree_root {
            // The worktree root isn't a row in this tree and isn't this app's to rename.
            return;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.tree_inline_edit = Some(TreeInlineEdit {
            kind: InlineEditKind::Rename { path, is_dir },
            // `seeded`, not `new()` + `set(..)`: the current name is this field's baseline, so
            // the first Ctrl+Z must not blank it - see `TextField::seeded`'s own docs.
            name: text_history::TextField::seeded(&name),
            error: None,
        });
        self.focus_file_tree(window, cx);
        cx.notify();
    }

    /// `F2`'s handler - renames whatever row is selected.
    pub(in crate::sidebar) fn start_tree_rename_for_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.selected_tree_path.clone() else {
            return;
        };
        if !path.starts_with(&self.file_tree_root) {
            return;
        }
        let is_dir = self
            .file_tree
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.is_dir)
            .unwrap_or_else(|| path.is_dir());
        self.start_tree_rename(path, is_dir, window, cx);
    }

    pub(in crate::sidebar) fn cancel_tree_inline_edit(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree_inline_edit.take().is_some() {
            cx.notify();
        }
    }

    /// The inline editor's key handler - append/backspace/Enter/Escape, the same minimal shape
    /// `crate::root::new_file::AdeApp::handle_new_file_key_down` established, including its
    /// "leave modified keystrokes unhandled so app-level shortcuts still work" rule.
    ///
    /// Also handles `Escape` for the context menu when no editor is open, so a keyboard-opened
    /// menu can be dismissed the same way a mouse-opened one can (issue #19 §1).
    pub(in crate::sidebar) fn handle_tree_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        if self.tree_inline_edit.is_none() {
            if keystroke.key == "escape" {
                // The delete confirmation is checked *first*: it is a modal on top of everything
                // else the tree can show, so one Escape must dismiss the thing in front, not
                // something behind it.
                if self.tree_delete_confirm.is_some() {
                    self.cancel_tree_delete(cx);
                    cx.stop_propagation();
                } else if self.tree_context_menu.is_some() {
                    self.close_tree_context_menu(cx);
                    cx.stop_propagation();
                }
            }
            return;
        }
        // GitHub issue #27's "solid mid-keystroke" - see `crate::palette::render::AdeApp::
        // handle_palette_key_down`'s identical reasoning for resetting unconditionally here,
        // before dispatching: every branch below is real typing/editing in this real input.
        self.reset_caret_blink(cx);
        match keystroke.key.as_str() {
            "escape" => {
                self.cancel_tree_inline_edit(window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.commit_tree_inline_edit(window, cx);
                cx.stop_propagation();
            }
            "backspace" => {
                if let Some(edit) = self.tree_inline_edit.as_mut() {
                    edit.name.pop(Instant::now());
                    edit.error = None;
                    cx.notify();
                    cx.stop_propagation();
                }
            }
            _ => {
                if let Some(text) = keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty())
                {
                    if let Some(edit) = self.tree_inline_edit.as_mut() {
                        edit.name.push_str(text, Instant::now());
                        edit.error = None;
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }
        }
    }

    /// `Ctrl/Cmd+Z` inside the inline name editor - the tree's half of GitHub issue #17's
    /// per-widget text undo.
    ///
    /// This surface and that feature were built on two branches in parallel and only met at the
    /// merge, which is exactly why this handler needs to exist rather than being assumed: with
    /// the editor open the tree's own node is the deepest focused one, so without the
    /// `"text-input"` context word `crate::sidebar::AdeApp::file_tree_shell` now emits, plain
    /// `Ctrl+Z` while typing a filename satisfied `Undo`'s `Some("!terminal && !text-input")`
    /// and ran the *worktree* history instead - discarding or re-committing real git state from
    /// inside a rename box. The tag makes that predicate unsatisfiable here; this listener is
    /// what the resulting `TextUndo` lands on. Registered on the same node that carries the tag
    /// and the focus handle, per `crate::default_key_bindings`' own rule.
    ///
    /// A no-op when no editor is open: the tag is only emitted while one is, so the action
    /// cannot normally be produced then, and an unconditional `cx.notify()` would repaint for
    /// nothing.
    pub(in crate::sidebar) fn handle_tree_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edit) = self.tree_inline_edit.as_mut() {
            if edit.name.undo() {
                // The rejection hint described the *old* text; it is stale the moment the text
                // moves, same as on every ordinary keystroke above.
                edit.error = None;
                cx.notify();
            }
        }
    }

    /// `Ctrl/Cmd+Shift+Z` / `Ctrl+Y` inside the inline name editor - the mirror of
    /// [`Self::handle_tree_text_undo`].
    pub(in crate::sidebar) fn handle_tree_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(edit) = self.tree_inline_edit.as_mut() {
            if edit.name.redo() {
                edit.error = None;
                cx.notify();
            }
        }
    }

    /// Enter: validates the typed name and performs the real filesystem operation. On a
    /// rejection the editor stays open with a real hint, exactly like the `+` menu's prompt.
    pub(in crate::sidebar) fn commit_tree_inline_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.tree_inline_edit.clone() else {
            return;
        };
        // The same one validator `crate::root::new_file::AdeApp::create_new_file` uses - not a
        // second copy of the rules.
        let name = match file_ops::validate_entry_name(edit.name.as_str()) {
            Ok(name) => name.to_string(),
            Err(message) => {
                if let Some(open) = self.tree_inline_edit.as_mut() {
                    open.error = Some(message);
                }
                cx.notify();
                return;
            }
        };

        match &edit.kind {
            InlineEditKind::NewFile { parent } => {
                let parent = parent.clone();
                // Delegates to the pre-existing, already-tested "New file" flow
                // (`crate::root::new_file::AdeApp::create_file_named`) rather than writing a
                // second empty file here: that path opens the file in a real tab, seeds a real
                // `EditBuffer`, reveals the row, and performs the first write through the same
                // freshness-gated save pipeline every other save uses.
                match self.create_file_named(&parent, &name, window, cx) {
                    Ok(()) => {
                        self.tree_inline_edit = None;
                        self.refresh_after_file_op(cx);
                    }
                    Err(message) => {
                        if let Some(open) = self.tree_inline_edit.as_mut() {
                            open.error = Some(message);
                        }
                        cx.notify();
                        return;
                    }
                }
            }
            InlineEditKind::NewFolder { parent } => {
                let destination = parent.join(&name);
                if destination.symlink_metadata().is_ok() {
                    if let Some(open) = self.tree_inline_edit.as_mut() {
                        open.error = Some(format!("\"{name}\" already exists"));
                    }
                    cx.notify();
                    return;
                }
                match std::fs::create_dir(&destination) {
                    Ok(()) => {
                        self.tree_inline_edit = None;
                        self.reveal_in_tree(&destination, cx);
                        self.selected_tree_path = Some(destination);
                        self.refresh_after_file_op(cx);
                    }
                    Err(err) => {
                        if let Some(open) = self.tree_inline_edit.as_mut() {
                            open.error = Some(err.to_string());
                        }
                        cx.notify();
                    }
                }
            }
            InlineEditKind::Rename { path, .. } => {
                let Some(parent) = path.parent().map(Path::to_path_buf) else {
                    self.tree_inline_edit = None;
                    cx.notify();
                    return;
                };
                let destination = parent.join(&name);
                if destination == *path {
                    self.tree_inline_edit = None;
                    cx.notify();
                    return;
                }
                match file_ops::move_path(path, &destination) {
                    Ok(()) => {
                        let old = path.clone();
                        self.tree_inline_edit = None;
                        self.rename_open_paths(&old, &destination, cx);
                        self.refresh_after_file_op(cx);
                    }
                    Err(err) => {
                        if let Some(open) = self.tree_inline_edit.as_mut() {
                            open.error = Some(err.to_string());
                        }
                        cx.notify();
                    }
                }
            }
        }
        cx.notify();
    }

    // ---------------------------------------------------------------- clipboard

    pub(in crate::sidebar) fn set_tree_clipboard(
        &mut self,
        path: PathBuf,
        mode: ClipboardMode,
        cx: &mut Context<Self>,
    ) {
        self.tree_clipboard = Some(TreeClipboard { path, mode });
        cx.notify();
    }

    /// `Ctrl+C`/`Ctrl+X`'s handler - acts on the selected row, and is a real no-op with nothing
    /// selected rather than silently capturing the root.
    pub(in crate::sidebar) fn copy_selection_to_tree_clipboard(
        &mut self,
        mode: ClipboardMode,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.selected_tree_path.clone() else {
            return;
        };
        if !path.starts_with(&self.file_tree_root) || path == self.file_tree_root {
            return;
        }
        self.set_tree_clipboard(path, mode, cx);
    }

    /// `Ctrl+V`'s handler - pastes into the selected folder, the selected file's own folder, or
    /// the worktree root.
    pub(in crate::sidebar) fn paste_into_selection(&mut self, cx: &mut Context<Self>) {
        let destination = match self.selected_tree_path.clone() {
            Some(path) if path.starts_with(&self.file_tree_root) => {
                let is_dir = self
                    .file_tree
                    .iter()
                    .find(|entry| entry.path == path)
                    .map(|entry| entry.is_dir)
                    .unwrap_or_else(|| path.is_dir());
                if is_dir {
                    path
                } else {
                    path.parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.file_tree_root.clone())
                }
            }
            _ => self.file_tree_root.clone(),
        };
        self.paste_into_dir(&destination, cx);
    }

    /// The real paste: a `Cut` moves, a `Copy` copies.
    ///
    /// A **copy**'s destination name is resolved by [`file_ops::unique_destination`], so pasting
    /// back into the folder something was copied from produces a real `name copy.ext` rather than
    /// an overwrite or a collision error (issue #19 §3).
    ///
    /// A **cut** deliberately does *not* auto-suffix against its own source. Cutting `util.rs`
    /// and pasting it into the folder it came from means "move it here", and it is already here -
    /// the honest answer is a no-op, not an unrequested rename to `util copy.rs` with no undo,
    /// which is what an unconditional `unique_destination` produced in an earlier version of this
    /// method (found in review). Against a *different* occupant of that name it still suffixes,
    /// since refusing outright would be worse than landing beside it.
    pub(in crate::sidebar) fn paste_into_dir(&mut self, dir: &Path, cx: &mut Context<Self>) {
        let Some(entry) = self.tree_clipboard.clone() else {
            return;
        };
        let Some(name) = entry
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            return;
        };

        if entry.mode == ClipboardMode::Cut && dir.join(&name) == entry.path {
            // Already exactly where it was asked to go.
            self.tree_clipboard = None;
            cx.notify();
            return;
        }

        let destination = match file_ops::unique_destination(dir, &name) {
            Ok(destination) => destination,
            Err(err) => return self.report_tree_op_error(err.to_string(), cx),
        };

        match entry.mode {
            // Off the foreground thread - see `Self::spawn_tree_copy`'s docs.
            ClipboardMode::Copy => self.spawn_tree_copy(entry.path, destination, cx),
            ClipboardMode::Cut => {
                // A single `rename` syscall, unlike the recursive copy above: kept synchronous so
                // the tab/buffer repair below lands in the same update as the move itself.
                if let Err(err) = file_ops::move_path(&entry.path, &destination) {
                    return self.report_tree_op_error(err.to_string(), cx);
                }
                // A cut is a move, so every open tab / buffer pointing at the old location has
                // to follow it, exactly as for a rename.
                self.rename_open_paths(&entry.path, &destination, cx);
                // The entry is no longer where the clipboard says it is; a second paste would
                // fail against a path that no longer exists.
                self.tree_clipboard = None;
                self.reveal_in_tree(&destination, cx);
                self.selected_tree_path = Some(destination);
                self.refresh_after_file_op(cx);
            }
        }
    }

    /// "Duplicate" - a copy next to the original, named by the same
    /// [`file_ops::unique_destination`] rule a paste-into-the-source-folder uses, so the two can
    /// never disagree about what a duplicate is called.
    pub(in crate::sidebar) fn duplicate_tree_entry(&mut self, path: &Path, cx: &mut Context<Self>) {
        let (Some(parent), Some(name)) = (
            path.parent(),
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned()),
        ) else {
            return;
        };
        let destination = match file_ops::unique_destination(parent, &name) {
            Ok(destination) => destination,
            Err(err) => return self.report_tree_op_error(err.to_string(), cx),
        };
        self.spawn_tree_copy(path.to_path_buf(), destination, cx);
    }

    /// Runs a real [`file_ops::copy_path`] on the background executor and applies the result on
    /// the foreground thread - the same "gather / compute / write back" shape
    /// [`AdeApp::load_file_tree`] and [`Self::confirm_tree_delete`] use.
    ///
    /// Background, not inline in the click handler, and that is a correctness point rather than a
    /// micro-optimization: `copy_path` recurses over a whole directory tree, so duplicating a
    /// `node_modules`-sized folder from a click listener would freeze the window for as long as
    /// the copy took. An earlier version of this method did exactly that, contradicting this
    /// module's sibling `confirm_tree_delete` (whose own docs insist on "never the foreground
    /// thread") - found in review.
    ///
    /// Nothing that could race a concurrent operation is captured: the destination name is
    /// resolved by the caller immediately before this runs, and a collision that appears in
    /// between is refused by `copy_path`'s own existence check rather than silently overwritten.
    fn spawn_tree_copy(&mut self, source: PathBuf, destination: PathBuf, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn({
                    let source = source.clone();
                    let destination = destination.clone();
                    async move {
                        file_ops::copy_path(&source, &destination).map_err(|err| err.to_string())
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => {
                        this.reveal_in_tree(&destination, cx);
                        this.selected_tree_path = Some(destination);
                        this.refresh_after_file_op(cx);
                    }
                    Err(message) => this.report_tree_op_error(message, cx),
                }
                cx.notify();
            });
        });
        self._tree_copy_task = Some(task);
    }

    /// Writes a path to the real system clipboard (`gpui::App::write_to_clipboard`) - absolute,
    /// or worktree-relative for "Copy Relative Path".
    pub(in crate::sidebar) fn copy_path_to_system_clipboard(
        &mut self,
        path: &Path,
        relative: bool,
        cx: &mut Context<Self>,
    ) {
        let text = if relative {
            path.strip_prefix(&self.file_tree_root)
                .unwrap_or(path)
                .display()
                .to_string()
        } else {
            path.display().to_string()
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    // ---------------------------------------------------------------- delete

    /// Arms the delete confirmation. **Never deletes anything itself** - the real removal only
    /// happens in [`Self::confirm_tree_delete`], which is reachable only from the confirmation
    /// panel's own button (issue #19 §3: "Delete asks for confirmation").
    ///
    /// The mechanism is resolved here, against a real `$PATH` probe, so the panel can name what
    /// will actually happen.
    pub(in crate::sidebar) fn request_tree_delete(
        &mut self,
        path: PathBuf,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        // The one hard boundary on the app's single irreversible operation: whatever route got
        // here, the path must be a plain, normal-component path genuinely inside the session's
        // worktree. Every real caller already satisfies this (the targets come from the tree's
        // own walk), which is exactly why it is worth stating as a checked precondition rather
        // than an assumption - a future caller that passes something else gets a refusal, not a
        // `remove_dir_all` outside the worktree.
        if !is_inside_worktree(&self.file_tree_root, &path) {
            return self.report_tree_op_error(
                format!(
                    "refusing to delete {}: it is not inside this worktree",
                    path.display()
                ),
                cx,
            );
        }
        let mechanism =
            file_ops::resolve_delete_mechanism(std::env::consts::OS, &path, |program| {
                pty_core::resolve_on_path(program).is_some()
            });
        self.tree_delete_confirm = Some(PendingTreeDelete {
            path,
            is_dir,
            mechanism,
        });
        cx.notify();
    }

    pub(in crate::sidebar) fn cancel_tree_delete(&mut self, cx: &mut Context<Self>) {
        if self.tree_delete_confirm.take().is_some() {
            cx.notify();
        }
    }

    /// Runs the confirmed delete. A trash-backed delete shells out to the resolved command on
    /// the background executor (never the foreground thread); a permanent one runs
    /// [`file_ops::delete_permanently`] there for the same reason.
    ///
    /// A failed trash command is reported as a real error and **does not** fall back to a
    /// permanent delete: the user confirmed "move to trash", and quietly escalating that into an
    /// irreversible removal because a command failed would be exactly the kind of dishonest
    /// convenience this app avoids.
    pub(in crate::sidebar) fn confirm_tree_delete(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.tree_delete_confirm.take() else {
            return;
        };
        let path = pending.path.clone();
        let mechanism = pending.mechanism.clone();
        let label = path.display().to_string();
        let task = cx.spawn(async move |this, cx| {
            let outcome =
                cx.background_executor()
                    .spawn({
                        let path = path.clone();
                        async move {
                            match mechanism {
                                DeleteMechanism::Trash { program, args } => {
                                    match std::process::Command::new(program).args(&args).status() {
                                        Ok(status) if status.success() => Ok(()),
                                        Ok(status) => Err(format!(
                                            "{program} exited with {status} while trashing {label}"
                                        )),
                                        Err(err) => Err(format!("failed to run {program}: {err}")),
                                    }
                                }
                                DeleteMechanism::Permanent => file_ops::delete_permanently(&path)
                                    .map_err(|err| err.to_string()),
                            }
                        }
                    })
                    .await;
            let _ = this.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => {
                        this.forget_deleted_paths(&path, cx);
                        this.refresh_after_file_op(cx);
                    }
                    Err(message) => this.report_tree_op_error(message, cx),
                }
                cx.notify();
            });
        });
        self._tree_delete_task = Some(task);
        cx.notify();
    }

    /// Drops the app state that pointed at a path that no longer exists - open tabs and their
    /// buffers, every path-keyed side table [`Self::rename_open_paths`] remaps, the LSP's own
    /// per-document bookkeeping, the selection, and this worktree's recorded expansion of a
    /// deleted folder (and of everything under it).
    ///
    /// Structurally the *mirror* of [`Self::rename_open_paths`], and reviewed as one: every field
    /// that method remaps, this one drops. An earlier version handled only tabs, buffers and the
    /// selection, which left a deleted file's `reviewed_files` entry, its
    /// `file_external_conflict` flag, its `file_save_error`, and - worst - its
    /// `lsp_opened_files`/`lsp_document_versions` entries behind. That last one is not cosmetic:
    /// `crate::lsp::client`'s `didOpen` dispatch early-returns for a path already in
    /// `lsp_opened_files`, so recreating a file at the deleted path would silently get no
    /// diagnostics and no completions for the rest of the session.
    ///
    /// Deliberately **not** a `close_file_tab` call: that method restores focus and picks a
    /// neighbouring tab, both of which need a `Window` this async completion handler doesn't
    /// have. It removes the tab entries directly and lets `open_change` fall back to whatever
    /// tab is still open, which is the same end state without the focus move.
    fn forget_deleted_paths(&mut self, deleted: &Path, cx: &mut Context<Self>) {
        let deleted_relative = self.worktree_relative(deleted);
        let under_relative = |path: &Path| file_ops::is_self_or_descendant(&deleted_relative, path);
        let under_absolute = |path: &Path| file_ops::is_self_or_descendant(deleted, path);

        self.open_files.retain(|open| !under_relative(open));
        self.edit_buffers.retain(|key, _| !under_relative(key));
        if self.open_change.as_deref().is_some_and(under_relative) {
            self.open_change = self.open_files.first().cloned();
        }
        if self
            .selected_tree_path
            .as_deref()
            .is_some_and(under_absolute)
        {
            self.selected_tree_path = None;
        }
        if self
            .tree_clipboard
            .as_ref()
            .is_some_and(|entry| under_absolute(&entry.path))
        {
            self.tree_clipboard = None;
        }

        // The same field list `Self::rename_open_paths` remaps - see this method's own docs.
        self.reviewed_files.retain(|path| !under_relative(path));
        self.file_external_conflict
            .retain(|path| !under_relative(path));
        self.file_save_pending.retain(|path| !under_relative(path));
        self.file_save_running.retain(|path| !under_relative(path));
        self._file_save_tasks
            .retain(|path, _| !under_relative(path));
        self._rehighlight_tasks
            .retain(|path, _| !under_relative(path));
        self._lsp_sync_tasks.retain(|path, _| !under_relative(path));
        if self
            .file_save_error
            .as_ref()
            .is_some_and(|(path, _)| under_relative(path))
        {
            self.file_save_error = None;
        }
        self.forget_lsp_document_state(&|path| under_absolute(path), &|path| under_relative(path));

        let stale: Vec<PathBuf> = self
            .expanded_dirs
            .iter()
            .filter(|dir| under_absolute(dir))
            .cloned()
            .collect();
        let mut changed = false;
        for dir in stale {
            changed |= self.record_dir_expanded(&dir, false);
        }
        if changed {
            self.persist_fold_state(cx);
        }
        self.invalidate_code_surface_caches();
        self.refresh_open_diff_file_cache();
    }

    /// `path` relative to the tree root, falling back to the path itself when it isn't inside -
    /// the one conversion both [`Self::rename_open_paths`] and [`Self::forget_deleted_paths`]
    /// use, since half of this app's path-keyed state is worktree-relative
    /// (`AdeApp::open_files`' own convention) and half is absolute, and mixing the two up is a
    /// silent no-op rather than an error.
    fn worktree_relative(&self, path: &Path) -> PathBuf {
        path.strip_prefix(&self.file_tree_root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf())
    }

    /// Drops every `crate::lsp::client` per-document entry for a path that has gone away.
    ///
    /// Split out because these six maps do not share a key space, and the split is not the one
    /// their names suggest - each field's own docs in `crate::root` are the authority and were
    /// read one by one for this:
    ///
    /// - **absolute**: `lsp_opened_files`, `lsp_document_versions`, `lsp_uri_cache`;
    /// - **worktree-relative** (`AdeApp::edit_buffers`' convention): `lsp_last_synced_content`,
    ///   `lsp_synced_version`, `lsp_diagnostics_confirmed_version` - the last two are documented
    ///   as "keyed the same worktree-relative way as `lsp_last_synced_content`", *not* the same
    ///   way as `lsp_document_versions` beside them.
    ///
    /// Passing one predicate for both groups would silently retain half of these, which is
    /// exactly the kind of no-op this whole method exists to avoid. `lsp_clients` itself is keyed
    /// by `(worktree root, language)` and so is untouched by a file-level rename or delete;
    /// `file_view_diagnostics` is keyed by line number and is dropped wholesale by
    /// [`Self::invalidate_code_surface_caches`]' callers instead.
    fn forget_lsp_document_state(
        &mut self,
        absolute: &dyn Fn(&Path) -> bool,
        relative: &dyn Fn(&Path) -> bool,
    ) {
        self.lsp_opened_files.retain(|path| !absolute(path));
        self.lsp_document_versions.retain(|path, _| !absolute(path));
        self.lsp_uri_cache.retain(|path, _| !absolute(path));
        self.lsp_last_synced_content
            .retain(|path, _| !relative(path));
        self.lsp_synced_version.retain(|path, _| !relative(path));
        self.lsp_diagnostics_confirmed_version
            .retain(|path, _| !relative(path));
    }

    // ---------------------------------------------------------------- bound actions

    /// `Shift+F10` (`crate::root::FileTreeContextMenu`).
    pub(in crate::sidebar) fn handle_file_tree_context_menu_action(
        &mut self,
        _action: &crate::root::FileTreeContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_tree_context_menu_from_keyboard(window, cx);
    }

    /// `F2` (`crate::root::FileTreeRename`).
    pub(in crate::sidebar) fn handle_file_tree_rename_action(
        &mut self,
        _action: &crate::root::FileTreeRename,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_tree_rename_for_selection(window, cx);
    }

    /// `Ctrl/⌘+C` while the tree is focused (`crate::root::FileTreeCopy`).
    pub(in crate::sidebar) fn handle_file_tree_copy_action(
        &mut self,
        _action: &crate::root::FileTreeCopy,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_selection_to_tree_clipboard(ClipboardMode::Copy, cx);
    }

    /// `Ctrl/⌘+X` while the tree is focused (`crate::root::FileTreeCut`).
    pub(in crate::sidebar) fn handle_file_tree_cut_action(
        &mut self,
        _action: &crate::root::FileTreeCut,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.copy_selection_to_tree_clipboard(ClipboardMode::Cut, cx);
    }

    /// `Ctrl/⌘+V` while the tree is focused (`crate::root::FileTreePaste`).
    pub(in crate::sidebar) fn handle_file_tree_paste_action(
        &mut self,
        _action: &crate::root::FileTreePaste,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.paste_into_selection(cx);
    }

    // ---------------------------------------------------------------- misc actions

    /// Collapses `dir` and every expanded folder beneath it, through issue #18's own real
    /// recording path ([`Self::record_dir_expanded`]) with a single queued write for the whole
    /// subtree - the same "one write for a whole ancestor chain" shape `Self::reveal_in_tree`
    /// already uses, rather than one write per level.
    pub(in crate::sidebar) fn collapse_subtree(&mut self, dir: &Path, cx: &mut Context<Self>) {
        let affected: Vec<PathBuf> = self
            .expanded_dirs
            .iter()
            .filter(|expanded| file_ops::is_self_or_descendant(dir, expanded))
            .cloned()
            .collect();
        let mut changed = false;
        for path in affected {
            changed |= self.record_dir_expanded(&path, false);
        }
        if changed {
            self.persist_fold_state(cx);
        }
        cx.notify();
    }

    /// "Reveal in file manager" - hands the containing directory to the OS default-open handler
    /// through the exact same real per-platform mechanism the Settings page's "Open file" button
    /// uses (`crate::settings::widgets`' `open_command_for`/`spawn_open_command`:
    /// `xdg-open`/`open`/`cmd /c start`), rather than a second implementation of it.
    ///
    /// A *file* is revealed by opening its parent directory: none of those three commands has a
    /// portable "select this entry" form, and handing a file path to `xdg-open` would open the
    /// file in its default application - a completely different action from the one the row
    /// promises.
    pub(in crate::sidebar) fn reveal_in_file_manager(
        &mut self,
        path: &Path,
        cx: &mut Context<Self>,
    ) {
        let directory = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.file_tree_root.clone())
        };
        self.open_path_with_os_handler(&directory, cx);
    }

    /// Carries every path-keyed piece of app state across a rename or a cut+paste (issue #19 §2:
    /// "open tabs / the diff view follow the renamed path - no orphaned buffers").
    ///
    /// Handles a renamed *directory* too, by prefix: every open tab, edit buffer, expanded
    /// folder and reviewed-file entry underneath it is remapped, not just an exact match.
    ///
    /// **Half of this app's path-keyed state is worktree-relative and half is absolute**, and
    /// getting one wrong is a silent no-op rather than a compile error (`strip_prefix` simply
    /// fails and the entry is left alone) - see the `reviewed_files` line below for the real bug
    /// that produced. Each field is remapped with the pair matching its own documented key space.
    /// [`Self::forget_deleted_paths`] is this method's mirror and must move field-for-field with
    /// it.
    ///
    /// The derived caches ([`AdeApp::file_view_cache`], the diff-highlight cache, the row layout
    /// map, the hover card, the completions popup) are *invalidated* rather than remapped: every
    /// one of them is keyed by the path it was computed for, and a rename changes the file's
    /// identity for `git` as well, so recomputing them against the new path is both simpler and
    /// the only way to get an honest answer.
    pub(in crate::sidebar) fn rename_open_paths(
        &mut self,
        old: &Path,
        new: &Path,
        cx: &mut Context<Self>,
    ) {
        let old_relative = self.worktree_relative(old);
        let new_relative = self.worktree_relative(new);

        for open in self.open_files.iter_mut() {
            if let Some(moved) = remap_path(open, &old_relative, &new_relative) {
                *open = moved;
            }
        }
        if let Some(open) = self.open_change.as_ref() {
            if let Some(moved) = remap_path(open, &old_relative, &new_relative) {
                self.open_change = Some(moved);
            }
        }

        // The buffer map is keyed by the worktree-relative path *and* each buffer carries the
        // absolute path an explicit save writes to - both have to move, or a save after a rename
        // would write the old file back into existence.
        let buffers = std::mem::take(&mut self.edit_buffers);
        self.edit_buffers = buffers
            .into_iter()
            .map(|(key, mut buffer)| {
                let key = remap_path(&key, &old_relative, &new_relative).unwrap_or(key);
                if let Some(moved) = remap_path(&buffer.path, old, new) {
                    buffer.path = moved;
                }
                (key, buffer)
            })
            .collect();

        // Worktree-*relative*, like `edit_buffers` above and unlike the absolute-keyed sets
        // further down: `reviewed_files` is keyed by `wt_core::diff::DiffFile::path`
        // (`Self::toggle_reviewed`'s own argument, straight off a Changes row). An earlier
        // version passed the absolute pair here, which made this a guaranteed silent no-op -
        // `strip_prefix` simply failed for every entry - so a file's reviewed checkbox quietly
        // reset on every rename. Found in review; the regression test below drives it.
        remap_path_set(&mut self.reviewed_files, &old_relative, &new_relative);
        remap_path_set(
            &mut self.file_external_conflict,
            &old_relative,
            &new_relative,
        );
        remap_path_set(&mut self.file_save_pending, &old_relative, &new_relative);
        // In-flight work for a path that no longer exists is dropped rather than remapped: a
        // save task holds the *old* absolute path internally, so letting it finish would recreate
        // the file under its old name.
        let moved_relative = |path: &Path| file_ops::is_self_or_descendant(&old_relative, path);
        let moved_absolute = |path: &Path| file_ops::is_self_or_descendant(old, path);
        self.file_save_running.retain(|path| !moved_relative(path));
        self._file_save_tasks
            .retain(|path, _| !moved_relative(path));
        self._rehighlight_tasks
            .retain(|path, _| !moved_relative(path));
        self._lsp_sync_tasks.retain(|path, _| !moved_relative(path));
        if let Some((path, message)) = self.file_save_error.take() {
            let path = remap_path(&path, &old_relative, &new_relative).unwrap_or(path);
            self.file_save_error = Some((path, message));
        }
        // The LSP's per-document bookkeeping is *dropped*, not remapped, and that is the honest
        // choice rather than the lazy one: `lsp_document_versions` counts `didChange`s for a
        // document identified by URI, and the server has never been told the old URI went away.
        // Carrying the old counter onto the new path would send a `didChange` for a document the
        // server has no `didOpen` for. Dropping them makes the next open of the renamed path a
        // real, fresh `didOpen` - see `Self::forget_lsp_document_state` for the two key spaces
        // involved.
        self.forget_lsp_document_state(&moved_absolute, &moved_relative);

        if let Some(selected) = self.selected_tree_path.as_ref() {
            if let Some(moved) = remap_path(selected, old, new) {
                self.selected_tree_path = Some(moved);
            }
        }
        if let Some(entry) = self.tree_clipboard.as_mut() {
            if let Some(moved) = remap_path(&entry.path, old, new) {
                entry.path = moved;
            }
        }

        // Fold state moves through issue #18's own real recording path, so the persisted file
        // follows the rename instead of keeping an entry for a folder that no longer exists.
        let moved_dirs: Vec<(PathBuf, PathBuf)> = self
            .expanded_dirs
            .iter()
            .filter_map(|dir| remap_path(dir, old, new).map(|moved| (dir.clone(), moved)))
            .collect();
        let mut changed = false;
        for (from, to) in moved_dirs {
            changed |= self.record_dir_expanded(&from, false);
            changed |= self.record_dir_expanded(&to, true);
        }
        if changed {
            self.persist_fold_state(cx);
        }

        self.invalidate_code_surface_caches();
        self.refresh_open_diff_file_cache();
    }

    /// Everything derived from "which file is open, and what was on disk for it" - dropped as a
    /// group whenever a file operation changes what those answers are. Shared by the rename and
    /// delete paths so neither can forget one of them.
    fn invalidate_code_surface_caches(&mut self) {
        self.file_view_cache = None;
        self.file_load_state = crate::code_surface::state::FileLoadState::Idle;
        self.file_view_last_freshness_check = None;
        self.diff_highlight_cache = None;
        self.file_view_row_layout.clear();
        self.file_view_last_layout = None;
        self.file_view_last_bounds = None;
        self.file_view_last_layout_for = None;
        self.hover = None;
        self.completions = None;
        self.pending_cursor_line = None;
    }

    /// Re-reads the tree and the diff after a real filesystem change.
    ///
    /// Both are genuinely necessary and neither is redundant, which is the honest answer to issue
    /// #19 §4's "do these operations just touch the filesystem, or do they need to trigger a
    /// refresh?": the operations *are* plain filesystem changes, so `git` sees them with no help
    /// at all - but nothing in this app polls the working tree for the sidebar. The file tree is
    /// only ever re-walked by an explicit `load_file_tree` (there is no filesystem watcher in
    /// this app), and the Changes list / diff view is only recomputed by an explicit `load_diff`
    /// (`crate::rail::render::AdeApp::start_status_polling`'s 3-second timer refreshes the
    /// *rail's* per-worktree summary, not `AdeApp::diff_state`). Without this call the row would
    /// stay on screen after a delete and the diff would keep showing the pre-rename file.
    pub(in crate::sidebar) fn refresh_after_file_op(&mut self, cx: &mut Context<Self>) {
        self.load_file_tree(self.file_tree_root.clone(), cx);
        self.load_diff(self.diff_root.clone(), cx);
    }

    /// Surfaces a real failure from a file operation next to the tree rather than dropping it
    /// into the log - the same "small, visible, honest error surface" convention
    /// [`AdeApp::file_save_error`] already follows for a failed save.
    pub(in crate::sidebar) fn report_tree_op_error(
        &mut self,
        message: String,
        cx: &mut Context<Self>,
    ) {
        log::warn!("file tree operation failed: {message}");
        self.tree_op_error = Some(message);
        cx.notify();
    }
}

/// Applies [`remap_path`] to every member of a path set in place.
fn remap_path_set(set: &mut HashSet<PathBuf>, old: &Path, new: &Path) {
    let moved: Vec<(PathBuf, PathBuf)> = set
        .iter()
        .filter_map(|path| remap_path(path, old, new).map(|moved| (path.clone(), moved)))
        .collect();
    for (from, to) in moved {
        set.remove(&from);
        set.insert(to);
    }
}

/// Whether `path` is a plain, normal-component-only path inside `root` - the guard every
/// tree-originated operation runs before touching the filesystem, so a `..` that somehow reached
/// one of these methods can't act outside the session's worktree.
pub(crate) fn is_inside_worktree(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    relative.components().count() > 0
        && relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Real, end-to-end coverage for GitHub issue #19 against a live `AdeApp` in a real GPUI test
/// window - the only level at which "the open tab followed the rename", "a single click deleted
/// nothing" and "this keystroke never reached the tree" are observable at all.
#[cfg(test)]
mod tree_ops_regression_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::settings::store as settings_store;
    use crate::sidebar::context_menu::ContextTarget;
    use gpui::TestAppContext;
    use std::fs;
    use tempfile::TempDir;

    fn secondary(key: &str) -> String {
        if cfg!(target_os = "macos") {
            format!("cmd-{key}")
        } else {
            format!("ctrl-{key}")
        }
    }

    /// `src/main.rs` + `src/util.rs` + a root-level `README.md`.
    fn seed(repo: &TempDir) {
        fs::create_dir_all(repo.path().join("src")).expect("mkdir");
        fs::write(repo.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        fs::write(repo.path().join("src/util.rs"), "pub fn u() {}\n").expect("write");
        fs::write(repo.path().join("README.md"), "hi\n").expect("write");
    }

    /// §2, the headline requirement: "open tabs / the diff view follow the renamed path - no
    /// orphaned buffers". Drives the real inline editor, not `rename_open_paths` directly.
    #[gpui::test]
    fn renaming_an_open_file_carries_its_tab_and_buffer_with_no_orphan_left_behind(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let old = repo.path().join("src/main.rs");
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(old.clone(), window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                app.edit_buffers.contains_key(Path::new("src/main.rs")),
                "premise: opening the file must have created a real buffer keyed by its \
                 worktree-relative path"
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.start_tree_rename(old.clone(), false, window, cx);
            app.tree_inline_edit.as_mut().expect("editor").name =
                text_history::TextField::seeded("renamed.rs");
            app.commit_tree_inline_edit(window, cx);
        });
        cx.run_until_parked();

        assert!(!old.exists(), "the old path must be gone from disk");
        assert!(repo.path().join("src/renamed.rs").exists());

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_files,
                vec![PathBuf::from("src/renamed.rs")],
                "the open tab must follow the rename"
            );
            assert_eq!(
                app.open_change.as_deref(),
                Some(Path::new("src/renamed.rs"))
            );
            assert!(
                !app.edit_buffers.contains_key(Path::new("src/main.rs")),
                "an orphaned buffer still keyed by the old path is exactly what this must not \
                 leave behind"
            );
            let buffer = app
                .edit_buffers
                .get(Path::new("src/renamed.rs"))
                .expect("the buffer must be re-keyed, not dropped");
            assert_eq!(
                buffer.path,
                repo.path().join("src/renamed.rs"),
                "the buffer's own absolute save path must move too - otherwise the next save \
                 would recreate the file under its old name"
            );
            assert_eq!(
                app.selected_tree_path.as_deref(),
                Some(repo.path().join("src/renamed.rs").as_path())
            );
        });
    }

    /// The subtree half of the same requirement: renaming a *folder* has to carry every tab and
    /// buffer underneath it, not just an exact path match.
    #[gpui::test]
    fn renaming_a_folder_carries_every_open_tab_underneath_it(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("src/main.rs"), window, cx);
            app.open_file_view(repo.path().join("src/util.rs"), window, cx);
            app.set_dir_expanded(repo.path().join("src"), true, cx);
        });
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_tree_rename(repo.path().join("src"), true, window, cx);
            app.tree_inline_edit.as_mut().expect("editor").name =
                text_history::TextField::seeded("lib");
            app.commit_tree_inline_edit(window, cx);
        });
        cx.run_until_parked();

        assert!(repo.path().join("lib/main.rs").exists());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.open_files,
                vec![PathBuf::from("lib/main.rs"), PathBuf::from("lib/util.rs")],
                "every tab under the renamed folder must follow it"
            );
            assert!(app.edit_buffers.contains_key(Path::new("lib/util.rs")));
            assert!(!app.edit_buffers.contains_key(Path::new("src/util.rs")));
            assert!(
                app.expanded_dirs.contains(&repo.path().join("lib"))
                    && !app.expanded_dirs.contains(&repo.path().join("src")),
                "the folder's own expanded state must move with it, or the renamed folder would \
                 silently snap shut"
            );
        });
    }

    /// §3: "Delete asks for confirmation". One click on the menu row must arm the confirmation
    /// and remove nothing at all.
    #[gpui::test]
    fn a_single_delete_click_only_arms_a_confirmation_and_removes_nothing(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let victim = repo.path().join("README.md");
        app.update_in(cx, |app, window, cx| {
            app.open_tree_context_menu(ContextTarget::File(victim.clone()), 40.0, 60.0, window, cx);
            app.run_tree_menu_action(MenuAction::Delete, window, cx);
        });
        cx.run_until_parked();

        assert!(
            victim.exists(),
            "a single click on Delete must never remove anything - it only asks"
        );
        app.read_with(cx, |app, _| {
            let pending = app
                .tree_delete_confirm
                .as_ref()
                .expect("the confirmation must be armed");
            assert_eq!(pending.path, victim);
            assert!(!pending.is_dir);
            assert!(
                app.tree_context_menu.is_none(),
                "the menu must close so the row can't be clicked a second time by accident"
            );
        });
    }

    /// The other half: the confirmation's own button really does delete, and the deleted file's
    /// tab and buffer go with it.
    ///
    /// The pending delete's mechanism is forced to `Permanent` after the real
    /// [`AdeApp::request_tree_delete`] has resolved it: on a developer machine with `gio`
    /// installed the real resolution is `Trash`, and running it here would move a test fixture
    /// into the developer's own `~/.local/share/Trash`. The resolution logic itself is covered
    /// purely in `crate::sidebar::file_ops`' own tests; what this test exercises is
    /// [`AdeApp::confirm_tree_delete`]'s real removal and state repair.
    #[gpui::test]
    fn a_confirmed_delete_really_removes_the_file_and_its_tab(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let victim = repo.path().join("src/util.rs");
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(victim.clone(), window, cx);
            app.request_tree_delete(victim.clone(), false, cx);
            app.tree_delete_confirm.as_mut().expect("armed").mechanism = DeleteMechanism::Permanent;
            app.confirm_tree_delete(cx);
        });
        cx.run_until_parked();

        assert!(
            !victim.exists(),
            "the confirmed delete must really remove it"
        );
        app.read_with(cx, |app, _| {
            assert!(
                !app.open_files.contains(&PathBuf::from("src/util.rs")),
                "the deleted file's tab must not survive it"
            );
            assert!(!app.edit_buffers.contains_key(Path::new("src/util.rs")));
            assert!(app.tree_delete_confirm.is_none());
        });
    }

    /// A guard on the app's one irreversible operation.
    #[gpui::test]
    fn deleting_something_outside_the_worktree_is_refused_outright(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("tempdir");
        fs::write(outside.path().join("precious.txt"), "keep me").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.request_tree_delete(outside.path().join("precious.txt"), false, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.tree_delete_confirm.is_none(),
                "a path outside the worktree must not even reach a confirmation"
            );
            assert!(app.tree_op_error.is_some(), "and it must say so");
        });
        assert!(outside.path().join("precious.txt").exists());
    }

    /// §3: "pasting into the source folder auto-suffixes the copy's name" - a real second file,
    /// never an overwrite and never a collision error.
    #[gpui::test]
    fn pasting_into_the_source_folder_creates_a_real_suffixed_copy(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let source = repo.path().join("src/main.rs");
        app.update(cx, |app, cx| {
            app.set_tree_clipboard(source.clone(), ClipboardMode::Copy, cx);
            app.paste_into_dir(&repo.path().join("src"), cx);
        });
        cx.run_until_parked();

        let copy = repo.path().join("src/main copy.rs");
        assert!(
            copy.exists(),
            "the paste must produce a real, suffixed file"
        );
        assert_eq!(fs::read_to_string(&copy).expect("read"), "fn main() {}\n");
        assert_eq!(
            fs::read_to_string(&source).expect("read"),
            "fn main() {}\n",
            "the original must be untouched"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.tree_op_error.is_none(),
                "a paste into the source folder is a normal, successful operation - not an \
                 error: {:?}",
                app.tree_op_error
            );
            assert!(
                app.tree_clipboard.is_some(),
                "a copy stays on the clipboard so it can be pasted again"
            );
        });

        // A second paste must not fail either - it steps to the next suffix.
        app.update(cx, |app, cx| {
            app.paste_into_dir(&repo.path().join("src"), cx);
        });
        cx.run_until_parked();
        assert!(repo.path().join("src/main copy 2.rs").exists());
    }

    /// A cut is a move, so it has to repair open tabs exactly like a rename does - and must
    /// clear the clipboard, since the entry is no longer where it said it was.
    #[gpui::test]
    fn cutting_and_pasting_moves_the_entry_and_its_open_tab(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        fs::create_dir(repo.path().join("dest")).expect("mkdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let source = repo.path().join("src/util.rs");
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(source.clone(), window, cx);
            app.set_tree_clipboard(source.clone(), ClipboardMode::Cut, cx);
            app.paste_into_dir(&repo.path().join("dest"), cx);
        });
        cx.run_until_parked();

        assert!(!source.exists());
        assert!(repo.path().join("dest/util.rs").exists());
        app.read_with(cx, |app, _| {
            assert!(app.open_files.contains(&PathBuf::from("dest/util.rs")));
            assert!(!app.edit_buffers.contains_key(Path::new("src/util.rs")));
            assert!(
                app.tree_clipboard.is_none(),
                "a cut is consumed by its paste - a second paste would target a path that no \
                 longer exists"
            );
        });
    }

    /// §4: "watcher refreshes must not clobber an in-progress inline rename/create editor".
    ///
    /// Drives the real race: an agent creates a file on disk, the tree is genuinely re-walked
    /// (`load_file_tree` - the one mechanism that replaces `AdeApp::file_tree` wholesale, and the
    /// one a filesystem watcher would drive), and the half-typed name must survive it, still
    /// painted as a real row.
    #[gpui::test]
    fn a_tree_reload_during_an_inline_rename_keeps_the_typed_text(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.set_dir_expanded(repo.path().join("src"), true, cx);
            app.start_tree_rename(repo.path().join("src/main.rs"), false, window, cx);
            app.tree_inline_edit.as_mut().expect("editor").name =
                text_history::TextField::seeded("half-typed");
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("file-tree-inline-edit").is_some(),
            "premise: the editor must be a real painted row before the reload"
        );

        // An agent CLI creating a file mid-session, followed by the real re-walk.
        fs::write(repo.path().join("src/agent-made.rs"), "// new\n").expect("write");
        app.update(cx, |app, cx| {
            let root = app.file_tree_root.clone();
            app.load_file_tree(root, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.file_tree
                    .iter()
                    .any(|entry| entry.name == "agent-made.rs"),
                "premise: the re-walk must genuinely have replaced the row list"
            );
            let edit = app
                .tree_inline_edit
                .as_ref()
                .expect("the editor must survive a walk that replaced every row");
            assert_eq!(edit.name.as_str(), "half-typed");
        });
        assert!(
            cx.debug_bounds("file-tree-inline-edit").is_some(),
            "and it must still be painted, re-anchored against the new row list"
        );
    }

    /// §1: the empty-area menu's "Collapse All" must be issue #18's *real* reset - the one that
    /// also clears this worktree's persisted entry - not a second mechanism that only empties the
    /// in-memory set. Asserted against the real on-disk fold-state file, which is the only thing
    /// that can tell the two apart.
    #[gpui::test]
    fn the_empty_area_collapse_all_clears_the_persisted_fold_state_too(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let state_dir = TempDir::new().expect("tempdir");
        let settings_path = state_dir.path().join("settings.toml");
        let fold_path = crate::sidebar::fold_state::fold_state_path_for(&settings_path);
        let (app, cx) = cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                repo.path().to_path_buf(),
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.set_dir_expanded(repo.path().join("src"), true, cx);
        });
        cx.run_until_parked();
        assert!(
            fs::read_to_string(&fold_path)
                .expect("the fold-state file must exist after a real expand")
                .contains("src"),
            "premise: the expansion is genuinely on disk before Collapse All runs"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_tree_context_menu(ContextTarget::Empty, 30.0, 30.0, window, cx);
            app.run_tree_menu_action(MenuAction::CollapseAll, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.expanded_dirs.is_empty(), "the live set must be cleared");
        });
        let on_disk = fs::read_to_string(&fold_path).expect("read fold state");
        assert!(
            !on_disk.contains("src"),
            "the menu's Collapse All must go through issue #18's own `collapse_all_dirs` - which \
             clears the *persisted* entry as well. A parallel implementation that only emptied \
             `expanded_dirs` would leave this behind and the expansion would come back on the \
             next launch. Got: {on_disk}"
        );
    }

    /// The positive half of the keyboard-access requirement (§1) - without this, the negative
    /// tests below could pass simply because the binding never works at all.
    #[gpui::test]
    fn shift_f10_with_the_tree_focused_opens_the_menu_for_the_selected_row(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.selected_tree_path = Some(repo.path().join("README.md"));
            app.focus_file_tree(window, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("shift-f10");
        app.read_with(cx, |app, _| {
            let menu = app
                .tree_context_menu
                .as_ref()
                .expect("shift-f10 with the tree focused must open the menu");
            assert_eq!(
                menu.target,
                ContextTarget::File(repo.path().join("README.md"))
            );
        });
    }

    /// §1 + the keystroke-scoping discipline: `Shift+F10` must not reach the tree while one of
    /// its own inline name editors has the keyboard - it would open a menu on top of the field
    /// the user is typing into.
    #[gpui::test]
    fn shift_f10_while_an_inline_editor_is_open_neither_opens_a_menu_nor_disturbs_the_name(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.selected_tree_path = Some(repo.path().join("README.md"));
            app.start_tree_rename(repo.path().join("README.md"), false, window, cx);
            app.tree_inline_edit.as_mut().expect("editor").name =
                text_history::TextField::seeded("in-progress");
        });
        cx.run_until_parked();

        cx.simulate_keystrokes("shift-f10");
        app.read_with(cx, |app, _| {
            assert!(
                app.tree_context_menu.is_none(),
                "the `!tree-editing` half of the binding's context predicate is what stops this"
            );
            assert_eq!(
                app.tree_inline_edit
                    .as_ref()
                    .expect("still open")
                    .name
                    .as_str(),
                "in-progress",
                "and the editor must be untouched"
            );
        });
    }

    /// **The merge regression this whole group exists for.** GitHub issue #19 (this file tree)
    /// and issue #17 (per-widget text undo) were built on two branches in parallel and only met
    /// at a merge. Nothing about that merge conflicted textually here, and the merged tree
    /// compiled and passed both sides' suites - but this editor was a bare `String` with no
    /// `"text-input"` key-context word, so while typing a filename plain `Ctrl+Z` satisfied
    /// `Undo`'s `Some("!terminal && !text-input")` and ran the **worktree** history: discarding
    /// or re-committing real git state from inside a rename box, with the name unchanged and no
    /// indication of what had happened. Verbatim the "a keystroke reaches the wrong handler" bug
    /// class `crate::default_key_bindings`' own docs catalogue.
    ///
    /// Asserts both halves, because either alone would pass against a wrong fix: the typed name
    /// really is undone (so `TextUndo` genuinely arrived and had a real history to act on), and
    /// `worktree_history_status` is *still* untouched (so the worktree undo genuinely never ran
    /// - the assertion that fails against the pre-merge-fix code, where it reads
    /// `Some("nothing to undo")`).
    #[gpui::test]
    fn ctrl_z_while_typing_a_name_in_the_tree_undoes_the_name_not_the_worktree_history(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_tree_new_entry(repo.path().to_path_buf(), false, window, cx);
        });
        cx.run_until_parked();

        cx.simulate_input("notes.txt");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit
                    .as_ref()
                    .expect("the editor must be open")
                    .name
                    .as_str(),
                "notes.txt",
                "sanity check: the editor must really be focused and receiving real keystrokes, \
                 or everything below would pass for the wrong reason"
            );
        });
        assert!(
            app.read_with(cx, |app, _| app.worktree_history_status.is_none()),
            "sanity check: nothing has touched the worktree history yet"
        );

        cx.simulate_keystrokes(&secondary("z"));

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit
                    .as_ref()
                    .expect("the editor must still be open")
                    .name
                    .as_str(),
                "",
                "Ctrl+Z must undo the name typed into this field - one uninterrupted burst of \
                 typing is one coalesced group, so a single undo clears it"
            );
            assert!(
                app.worktree_history_status.is_none(),
                "and it must never have reached the worktree-level Undo: that is what the \
                 `\"text-input\"` word this editor's key context now carries makes impossible, \
                 by rendering `Undo`'s own `!text-input` predicate unsatisfiable here"
            );
        });
    }

    /// The other direction, which is what keeps the fix above honest rather than a blanket tag:
    /// the tree with **no** editor open is not a text surface, so `Ctrl+Z` there must still reach
    /// the worktree history exactly as it did before issue #17 existed.
    ///
    /// Without this, adding `"text-input"` unconditionally to the tree shell would pass the test
    /// above while silently killing the worktree undo for anyone whose focus happens to sit on
    /// the file tree - a new instance of the same bug class, pointed the other way.
    #[gpui::test]
    fn ctrl_z_in_the_focused_tree_with_no_editor_open_still_reaches_the_worktree_undo(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.selected_tree_path = Some(repo.path().join("README.md"));
            app.focus_file_tree(window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(
                app.tree_inline_edit.is_none(),
                "sanity check: no inline editor, so no `\"text-input\"` word"
            );
            assert!(app.worktree_history_status.is_none());
        });

        cx.simulate_keystrokes(&secondary("z"));

        assert_eq!(
            app.read_with(cx, |app, _| app.worktree_history_status.clone()),
            Some("nothing to undo".to_string()),
            "with the tree focused but nothing being typed, secondary-z must still reach the \
             worktree-level Undo and produce its real, honest status"
        );
    }

    /// The rename editor opens *pre-filled* with the entry's current name, and that pre-fill is
    /// the field's baseline rather than an edit to it - so the first `Ctrl+Z` must do nothing at
    /// all, not blank the box to `""`.
    ///
    /// This is what `crate::text_history::TextField::seeded` exists for; building the field as
    /// `new()` + `set(current_name)` would record the pre-fill as a real undoable step and leave
    /// the user in a state they never typed and cannot type their way back to (the name is gone
    /// and only redo returns it). Also asserts the keystroke did not fall through to the
    /// worktree history when the text field had nothing to give it.
    #[gpui::test]
    fn the_first_ctrl_z_in_a_rename_editor_does_not_blank_the_prefilled_name(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_tree_rename(repo.path().join("README.md"), false, window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit.as_ref().expect("editor").name.as_str(),
                "README.md",
                "sanity check: the rename editor really does open pre-filled"
            );
            assert!(
                !app.tree_inline_edit
                    .as_ref()
                    .expect("editor")
                    .name
                    .can_undo(),
                "and the pre-fill must not itself be a recorded, undoable step - this is the \
                 assertion that discriminates `TextField::seeded` from `new()` + `set(name)`, \
                 rather than inferring it from the text merely not having changed"
            );
        });

        cx.simulate_keystrokes(&secondary("z"));

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit.as_ref().expect("editor").name.as_str(),
                "README.md",
                "the pre-filled name is this field's baseline, not an undoable edit"
            );
            assert!(
                app.worktree_history_status.is_none(),
                "and an empty text history must not let the keystroke fall through to the \
                 worktree undo - the two systems are disjoint by context, not by whether the \
                 text one happens to have anything to do"
            );
        });
    }

    /// The redo half of the same routing, on both real spellings this app binds
    /// (`secondary-shift-z` and `ctrl-y`).
    #[gpui::test]
    fn redo_in_the_tree_inline_editor_restores_the_undone_name(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_tree_new_entry(repo.path().to_path_buf(), true, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_input("assets");
        cx.simulate_keystrokes(&secondary("z"));
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit.as_ref().expect("editor").name.as_str(),
                ""
            );
        });

        cx.simulate_keystrokes(&secondary("shift-z"));
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit.as_ref().expect("editor").name.as_str(),
                "assets",
                "secondary-shift-z must redo the tree editor's own text"
            );
            assert!(app.worktree_history_status.is_none());
        });

        // The second real spelling, which this app binds for the same action. The intermediate
        // assertion is load-bearing: without it, a `ctrl-y` that did nothing *and* a
        // `secondary-z` that did nothing would leave "assets" in place and pass.
        cx.simulate_keystrokes(&secondary("z"));
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit.as_ref().expect("editor").name.as_str(),
                "",
                "sanity check: the undo before the ctrl-y really did take effect"
            );
        });
        cx.simulate_keystrokes("ctrl-y");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.tree_inline_edit.as_ref().expect("editor").name.as_str(),
                "assets",
                "ctrl-y must reach the same handler"
            );
            assert!(app.worktree_history_status.is_none());
        });
    }

    /// `crate::keymap_overrides::file_tree_key_context`'s `(true, true)` arm - an inline name
    /// editor open *and* the delete confirmation armed - is enumerated in `real_context_stacks()`
    /// as a deliberate over-approximation. This asserts the reason it is safe to leave the
    /// `"text-input"` word on that arm: the state has no real gesture path into it.
    ///
    /// Arming a delete goes through the context menu, and [`AdeApp::open_tree_context_menu`]
    /// cancels any open inline editor first. Both audits of the merge that added the tag raised
    /// this arm - keeping `"text-input"` there means `Ctrl+Z` behind the scrim would edit a name
    /// field the user cannot see, but *dropping* it would hand the keystroke to the worktree
    /// `Undo` instead, which is strictly worse (it mutates real git state), and guarding the
    /// handlers would silently swallow the keystroke, which is this project's most-repeated bug
    /// class. Proving the state unreachable is what makes all three concerns moot; asserting it
    /// is what stops a later refactor from quietly making it reachable.
    #[gpui::test]
    fn arming_a_delete_is_not_reachable_while_an_inline_name_editor_is_open(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_tree_rename(repo.path().join("README.md"), false, window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.tree_inline_edit.is_some(), "sanity check: editor open");
            assert!(app.tree_delete_confirm.is_none());
        });

        // The only real way to arm a delete: open the row's context menu first.
        app.update_in(cx, |app, window, cx| {
            app.open_tree_context_menu(
                ContextTarget::File(repo.path().join("README.md")),
                10.0,
                10.0,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.tree_inline_edit.is_none(),
                "opening the context menu must cancel the inline editor - that is what makes the \
                 \"editor open + delete armed\" context stack unreachable, and so makes leaving \
                 `\"text-input\"` on that arm safe"
            );
        });
    }

    /// The same discipline for the clipboard bindings, and the highest-stakes case: `Ctrl+C` in a
    /// focused terminal is SIGINT, and a version of this feature that intercepted it would make
    /// it impossible to interrupt a running agent CLI.
    ///
    /// What this test proves, precisely: with a terminal focused, none of the three clipboard
    /// keystrokes reaches the tree's own handler. It does **not** prove the keystroke still
    /// reached the pty - that is a separate property, guaranteed by *where* the handlers are
    /// registered (see this module's own docs, point 1) and covered by
    /// `crate::terminal::pane`'s own `keystroke_to_bytes` tests. Stated explicitly because this
    /// test was checked against a deliberately un-scoped (`None`) binding and still passed: with
    /// the handlers on the tree's own node, an unmatched *handler* is what saves the terminal
    /// there, not the context predicate. The predicate's own load-bearing half is verified by
    /// the two tests above and by
    /// [`every_file_tree_binding_is_scoped_away_from_the_inline_editor`].
    #[gpui::test]
    fn ctrl_c_with_a_focused_terminal_never_reaches_the_trees_clipboard(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            // A real selection, so the tree binding would genuinely have something to copy if it
            // fired - otherwise this test could pass for the wrong reason.
            app.selected_tree_path = Some(repo.path().join("README.md"));
            app.sessions.focus_active(window, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes(&secondary("c"));
        cx.simulate_keystrokes(&secondary("x"));
        cx.simulate_keystrokes(&secondary("v"));
        app.read_with(cx, |app, _| {
            assert!(
                app.tree_clipboard.is_none(),
                "the tree's clipboard bindings are scoped to the `file-tree` context precisely \
                 so a focused terminal keeps its own control bytes"
            );
        });
    }

    /// And the positive half of *that* pair: with the tree focused, the same keystroke really
    /// does work.
    #[gpui::test]
    fn ctrl_c_with_the_tree_focused_copies_the_selected_entry(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.selected_tree_path = Some(repo.path().join("README.md"));
            app.focus_file_tree(window, cx);
        });
        cx.run_until_parked();

        cx.simulate_keystrokes(&secondary("c"));
        app.read_with(cx, |app, _| {
            let entry = app
                .tree_clipboard
                .as_ref()
                .expect("ctrl-c with the tree focused must copy the selection");
            assert_eq!(entry.path, repo.path().join("README.md"));
            assert_eq!(entry.mode, ClipboardMode::Copy);
        });
    }

    /// Structural, read off the *real* registered bindings rather than a hand-copied list (the
    /// same discipline `crate::settings::state::keybinding_rows` follows): every file-tree
    /// action must be scoped to a context that excludes both modal states. A future edit that
    /// widened one of them to `None`, or dropped either negated half, would silently start
    /// swallowing keystrokes typed into the tree's own inline name editor, or firing behind the
    /// delete confirmation's own scrim.
    #[test]
    fn every_file_tree_binding_is_scoped_away_from_the_inline_editor() {
        let expected = "file-tree && !tree-editing && !tree-delete-confirm";
        let tree_actions = [
            "app::FileTreeContextMenu",
            "app::FileTreeRename",
            "app::FileTreeCopy",
            "app::FileTreeCut",
            "app::FileTreePaste",
        ];
        let bindings = crate::default_key_bindings();
        let mut seen = Vec::new();
        for binding in &bindings {
            let name = binding.action().name();
            if !tree_actions.contains(&name) {
                continue;
            }
            seen.push(name);
            let predicate = binding
                .predicate()
                .unwrap_or_else(|| panic!("{name} must not be globally bound"))
                .to_string();
            assert_eq!(
                predicate, expected,
                "{name} is bound with the wrong context predicate"
            );
        }
        seen.sort_unstable();
        let mut want = tree_actions;
        want.sort_unstable();
        assert_eq!(
            seen, want,
            "every file-tree action must have a real registered binding"
        );
    }

    /// §1: a real right-click on a real painted row opens that row's own menu (not the empty-area
    /// one), positioned inside the window.
    #[gpui::test]
    fn right_clicking_a_folder_row_opens_the_folder_menu_at_a_clamped_origin(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let row = cx
            .debug_bounds("file-tree-row-src")
            .expect("the folder row must be painted");
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position: row.center(),
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();

        let viewport = cx.update(|window, _cx| window.bounds().size);
        app.read_with(cx, |app, _| {
            let menu = app
                .tree_context_menu
                .as_ref()
                .expect("a real right-click on a folder row must open a menu");
            assert_eq!(
                menu.target,
                ContextTarget::Folder(repo.path().join("src")),
                "the row's own handler must win over the container's empty-area one"
            );
            let rows = context_menu::menu_items(&menu.target, false).len();
            assert!(
                menu.origin_x >= 0.0
                    && menu.origin_x + context_menu::MENU_WIDTH <= f32::from(viewport.width),
                "the popover must sit inside the window"
            );
            assert!(
                menu.origin_y >= 0.0
                    && menu.origin_y + context_menu::menu_height(rows)
                        <= f32::from(viewport.height)
            );
        });
        assert!(
            cx.debug_bounds("tree-context-menu").is_some(),
            "and it must genuinely paint"
        );
    }

    /// §1's dismissal requirement, driven through the real key handler.
    #[gpui::test]
    fn escape_dismisses_the_context_menu(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_tree_context_menu(ContextTarget::Empty, 20.0, 20.0, window, cx);
        });
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        app.read_with(cx, |app, _| assert!(app.tree_context_menu.is_none()));
    }

    /// §2: New Folder really creates a directory, and the inline editor is what drives it.
    #[gpui::test]
    fn the_new_folder_editor_creates_a_real_directory(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_tree_context_menu(
                ContextTarget::Folder(repo.path().join("src")),
                20.0,
                20.0,
                window,
                cx,
            );
            app.run_tree_menu_action(MenuAction::NewFolder, window, cx);
            app.tree_inline_edit.as_mut().expect("editor").name =
                text_history::TextField::seeded("nested");
            app.commit_tree_inline_edit(window, cx);
        });
        cx.run_until_parked();

        assert!(repo.path().join("src/nested").is_dir());
        app.read_with(cx, |app, _| {
            assert!(app.tree_inline_edit.is_none(), "the editor must close");
            assert!(
                app.expanded_dirs.contains(&repo.path().join("src")),
                "the parent must have been expanded so the editor was visible in the first place"
            );
        });
    }

    /// §2: "invalid names are rejected with a hint" - and the editor stays open so the name can
    /// be corrected, exactly like the `+` menu's prompt.
    #[gpui::test]
    fn an_invalid_name_is_refused_with_a_real_hint_and_the_editor_stays_open(
        cx: &mut TestAppContext,
    ) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        for bad in ["", "  ", "a/b", ".."] {
            app.update_in(cx, |app, window, cx| {
                app.start_tree_new_entry(repo.path().to_path_buf(), true, window, cx);
                app.tree_inline_edit.as_mut().expect("editor").name =
                    text_history::TextField::seeded(bad);
                app.commit_tree_inline_edit(window, cx);
            });
            cx.run_until_parked();
            app.read_with(cx, |app, _| {
                let edit = app
                    .tree_inline_edit
                    .as_ref()
                    .unwrap_or_else(|| panic!("the editor must stay open for {bad:?}"));
                assert!(
                    edit.error.is_some(),
                    "{bad:?} must be rejected with a real hint"
                );
            });
        }

        // A name that collides with something already there is refused the same way.
        app.update_in(cx, |app, window, cx| {
            app.start_tree_new_entry(repo.path().to_path_buf(), true, window, cx);
            app.tree_inline_edit.as_mut().expect("editor").name =
                text_history::TextField::seeded("src");
            app.commit_tree_inline_edit(window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app
                .tree_inline_edit
                .as_ref()
                .expect("still open")
                .error
                .as_deref()
                .is_some_and(|error| error.contains("already exists")));
        });
        assert!(
            repo.path().join("src/main.rs").exists(),
            "and the existing folder must be untouched"
        );
    }

    /// §1's "Collapse Subtree": only that folder's own chain, never a sibling's.
    #[gpui::test]
    fn collapse_subtree_collapses_only_that_folders_own_descendants(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        fs::create_dir_all(repo.path().join("a/inner")).expect("mkdir");
        fs::create_dir_all(repo.path().join("b")).expect("mkdir");
        fs::write(repo.path().join("a/inner/x.rs"), "x").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.set_dir_expanded(repo.path().join("a"), true, cx);
            app.set_dir_expanded(repo.path().join("a/inner"), true, cx);
            app.set_dir_expanded(repo.path().join("b"), true, cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.collapse_subtree(&repo.path().join("a"), cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(!app.expanded_dirs.contains(&repo.path().join("a")));
            assert!(
                !app.expanded_dirs.contains(&repo.path().join("a/inner")),
                "a nested expansion left behind would reappear the moment the parent reopened"
            );
            assert!(
                app.expanded_dirs.contains(&repo.path().join("b")),
                "a sibling subtree must be untouched"
            );
        });
    }

    /// "Copy Relative Path" really writes to the real system clipboard, and really is relative.
    #[gpui::test]
    fn copy_relative_path_writes_the_worktree_relative_path(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.copy_path_to_system_clipboard(&repo.path().join("src/main.rs"), true, cx);
        });
        let text = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(text.as_deref(), Some("src/main.rs"));

        app.update(cx, |app, cx| {
            app.copy_path_to_system_clipboard(&repo.path().join("src/main.rs"), false, cx);
        });
        let text = cx.update(|_window, cx| cx.read_from_clipboard().and_then(|item| item.text()));
        assert_eq!(
            text.as_deref(),
            Some(
                repo.path()
                    .join("src/main.rs")
                    .display()
                    .to_string()
                    .as_str()
            )
        );
    }

    /// Every one of these bugs was found by this change's own adversarial review; each test was
    /// confirmed to fail against the code as it stood before the fix.
    mod review_findings {
        use super::*;
        use crate::sidebar::render::RightSidebarView;

        /// `reviewed_files` is keyed by `wt_core::diff::DiffFile::path` - worktree-*relative* -
        /// while the paths a rename works in are absolute. Remapping it with the absolute pair
        /// was a guaranteed silent no-op (`strip_prefix` failed for every entry), so a file's
        /// reviewed checkbox quietly reset on every rename and every cut+paste.
        #[gpui::test]
        fn a_rename_carries_the_files_reviewed_checkbox_with_it(cx: &mut TestAppContext) {
            let repo = TempDir::new().expect("tempdir");
            seed(&repo);
            let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
            cx.run_until_parked();

            app.update(cx, |app, cx| {
                app.toggle_reviewed(PathBuf::from("src/main.rs"), cx);
            });
            app.update_in(cx, |app, window, cx| {
                app.start_tree_rename(repo.path().join("src/main.rs"), false, window, cx);
                app.tree_inline_edit.as_mut().expect("editor").name =
                    text_history::TextField::seeded("renamed.rs");
                app.commit_tree_inline_edit(window, cx);
            });
            cx.run_until_parked();

            app.read_with(cx, |app, _| {
                assert!(
                    app.reviewed_files.contains(Path::new("src/renamed.rs")),
                    "the reviewed mark must follow the rename - got {:?}",
                    app.reviewed_files
                );
                assert!(!app.reviewed_files.contains(Path::new("src/main.rs")));
            });
        }

        /// `crate::lsp::client`'s `didOpen` dispatch early-returns for a path already in
        /// `lsp_opened_files`, and that set is documented as never being cleared on close. So a
        /// rename (or a delete) that left the old absolute path in it meant recreating a file at
        /// that path silently got no diagnostics and no completions for the rest of the session.
        #[gpui::test]
        fn renaming_and_deleting_both_clear_the_lsp_per_document_bookkeeping(
            cx: &mut TestAppContext,
        ) {
            let repo = TempDir::new().expect("tempdir");
            seed(&repo);
            let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
            cx.run_until_parked();

            let absolute = repo.path().join("src/main.rs");
            let relative = PathBuf::from("src/main.rs");
            app.update(cx, |app, _cx| {
                app.lsp_opened_files.insert(absolute.clone());
                app.lsp_document_versions.insert(absolute.clone(), 7);
                app.lsp_last_synced_content
                    .insert(relative.clone(), "fn main() {}\n".to_string());
                app.lsp_synced_version.insert(relative.clone(), 7);
            });

            app.update_in(cx, |app, window, cx| {
                app.start_tree_rename(absolute.clone(), false, window, cx);
                app.tree_inline_edit.as_mut().expect("editor").name =
                    text_history::TextField::seeded("renamed.rs");
                app.commit_tree_inline_edit(window, cx);
            });
            cx.run_until_parked();

            app.read_with(cx, |app, _| {
                assert!(
                    !app.lsp_opened_files.contains(&absolute),
                    "a stale `didOpen` record for the old path silently disables diagnostics \
                     for anything later created there"
                );
                assert!(!app.lsp_document_versions.contains_key(&absolute));
                assert!(!app.lsp_last_synced_content.contains_key(&relative));
                assert!(!app.lsp_synced_version.contains_key(&relative));
            });

            // And the delete half, on the renamed path.
            let renamed = repo.path().join("src/renamed.rs");
            let renamed_relative = PathBuf::from("src/renamed.rs");
            app.update(cx, |app, _cx| {
                app.lsp_opened_files.insert(renamed.clone());
                app.lsp_synced_version.insert(renamed_relative.clone(), 3);
            });
            app.update(cx, |app, cx| {
                app.request_tree_delete(renamed.clone(), false, cx);
                app.tree_delete_confirm.as_mut().expect("armed").mechanism =
                    DeleteMechanism::Permanent;
                app.confirm_tree_delete(cx);
            });
            cx.run_until_parked();

            app.read_with(cx, |app, _| {
                assert!(!app.lsp_opened_files.contains(&renamed));
                assert!(!app.lsp_synced_version.contains_key(&renamed_relative));
            });
        }

        /// Cutting a file and pasting it back into the folder it came from means "move it here",
        /// and it is already here. An unconditional `unique_destination` turned that into an
        /// unrequested rename to `util copy.rs`, with no error and no undo.
        #[gpui::test]
        fn cutting_and_pasting_into_the_source_folder_is_a_no_op_not_a_rename(
            cx: &mut TestAppContext,
        ) {
            let repo = TempDir::new().expect("tempdir");
            seed(&repo);
            let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
            cx.run_until_parked();

            let source = repo.path().join("src/util.rs");
            app.update(cx, |app, cx| {
                app.set_tree_clipboard(source.clone(), ClipboardMode::Cut, cx);
                app.paste_into_dir(&repo.path().join("src"), cx);
            });
            cx.run_until_parked();

            assert!(
                source.exists(),
                "the file must still be exactly where it was"
            );
            assert!(
                !repo.path().join("src/util copy.rs").exists(),
                "a cut back into its own folder must never silently rename the file"
            );
            app.read_with(cx, |app, _| {
                assert!(app.tree_clipboard.is_none(), "the cut is still consumed");
                assert!(app.tree_op_error.is_none(), "and it is not an error either");
            });
        }

        /// Switching to the Changes tab unrenders `file_tree_shell` - and with it the node
        /// `tree_focus_handle` is tracked on. Leaving `Window::focus` there makes GPUI fall back
        /// to the dispatch root, silently killing every keybinding until the next click: this
        /// project's single most-repeated bug class.
        #[gpui::test]
        fn leaving_the_files_tab_does_not_leave_focus_dangling_on_the_tree(
            cx: &mut TestAppContext,
        ) {
            let repo = TempDir::new().expect("tempdir");
            seed(&repo);
            let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
            cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
            cx.run_until_parked();

            app.update_in(cx, |app, window, cx| {
                app.open_tree_context_menu(
                    ContextTarget::File(repo.path().join("README.md")),
                    30.0,
                    30.0,
                    window,
                    cx,
                );
                app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
            });
            cx.run_until_parked();

            app.read_with(cx, |app, _| {
                assert!(
                    app.tree_context_menu.is_none(),
                    "a menu targeting a row that is no longer rendered must not survive the switch"
                );
            });

            let key = if cfg!(target_os = "macos") {
                "cmd-k"
            } else {
                "ctrl-k"
            };
            cx.simulate_keystrokes(key);
            assert!(
                app.read_with(cx, |app, _| app.palette_open),
                "a real {key} after switching away from the Files tab must still open the \
                 palette - before the fix, focus was left dangling on the now-unrendered tree"
            );
        }

        /// The delete confirmation is a modal. While it is up, the tree's own bindings must not
        /// fire behind its scrim, and Escape must dismiss it.
        #[gpui::test]
        fn the_delete_confirmation_is_modal_to_the_trees_own_keybindings(cx: &mut TestAppContext) {
            let repo = TempDir::new().expect("tempdir");
            seed(&repo);
            let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
            cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
            cx.run_until_parked();

            app.update_in(cx, |app, window, cx| {
                app.selected_tree_path = Some(repo.path().join("README.md"));
                app.focus_file_tree(window, cx);
                app.request_tree_delete(repo.path().join("README.md"), false, cx);
            });
            cx.run_until_parked();

            cx.simulate_keystrokes("f2");
            cx.simulate_keystrokes("shift-f10");
            app.read_with(cx, |app, _| {
                assert!(
                    app.tree_inline_edit.is_none() && app.tree_context_menu.is_none(),
                    "F2/Shift+F10 must not fire behind the modal's own scrim"
                );
                assert!(app.tree_delete_confirm.is_some(), "and it stays up");
            });

            cx.simulate_keystrokes("escape");
            app.read_with(cx, |app, _| {
                assert!(
                    app.tree_delete_confirm.is_none(),
                    "escape must dismiss the confirmation"
                );
            });
            assert!(repo.path().join("README.md").exists());
        }
    }

    /// A worktree switch must not leave a cut entry, a half-typed name, an open menu or an armed
    /// delete pointing at the worktree just left.
    #[gpui::test]
    fn switching_worktrees_clears_every_tree_operation_in_flight(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        seed(&repo);
        let other = TempDir::new().expect("tempdir");
        fs::write(other.path().join("elsewhere.txt"), "x").expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        // A directly-seeded `worktrees` list - the same pattern `crate::code_surface::zoom`'s own
        // per-worktree-reset test uses; `select_worktree` only needs a real, readable path.
        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                crate::rail::worktrees::WorktreeItem {
                    path: repo.path().to_path_buf(),
                    label: "wt-a".to_string(),
                    branch: None,
                    is_main: true,
                    is_locked: false,
                    error: None,
                },
                crate::rail::worktrees::WorktreeItem {
                    path: other.path().to_path_buf(),
                    label: "wt-b".to_string(),
                    branch: None,
                    is_main: false,
                    is_locked: false,
                    error: None,
                },
            ];
        });

        app.update_in(cx, |app, window, cx| {
            app.set_tree_clipboard(repo.path().join("README.md"), ClipboardMode::Cut, cx);
            // Order matters: `open_tree_context_menu` cancels an open inline editor, so opening
            // the menu *first* is the only way to have all four states live at once. An earlier
            // version of this test did it the other way round, which made the
            // `tree_inline_edit.is_none()` assertion below already true before `select_worktree`
            // ever ran - it would have passed against a reset that dropped that field entirely.
            app.open_tree_context_menu(ContextTarget::Empty, 10.0, 10.0, window, cx);
            app.start_tree_rename(repo.path().join("README.md"), false, window, cx);
            app.request_tree_delete(repo.path().join("README.md"), false, cx);
            assert!(
                app.tree_clipboard.is_some()
                    && app.tree_inline_edit.is_some()
                    && app.tree_context_menu.is_some()
                    && app.tree_delete_confirm.is_some(),
                "premise: all four must genuinely be live before the switch"
            );
            app.select_worktree(1, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.tree_clipboard.is_none());
            assert!(app.tree_inline_edit.is_none());
            assert!(app.tree_context_menu.is_none());
            assert!(app.tree_delete_confirm.is_none());
        });
        assert!(repo.path().join("README.md").exists());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remapping_moves_a_path_and_its_whole_subtree() {
        let old = Path::new("/repo/src");
        let new = Path::new("/repo/lib");
        assert_eq!(remap_path(old, old, new), Some(PathBuf::from("/repo/lib")));
        assert_eq!(
            remap_path(Path::new("/repo/src/inner/a.rs"), old, new),
            Some(PathBuf::from("/repo/lib/inner/a.rs"))
        );
        assert_eq!(remap_path(Path::new("/repo/other.rs"), old, new), None);
        // A sibling whose name merely *starts with* the renamed one must not be caught:
        // `Path::strip_prefix` is component-wise, not textual.
        assert_eq!(remap_path(Path::new("/repo/srcs/a.rs"), old, new), None);
    }

    #[test]
    fn only_plain_paths_inside_the_worktree_are_operable() {
        let root = Path::new("/repo");
        assert!(is_inside_worktree(root, Path::new("/repo/src/main.rs")));
        assert!(!is_inside_worktree(root, Path::new("/repo")));
        assert!(!is_inside_worktree(root, Path::new("/elsewhere/a.rs")));
        assert!(!is_inside_worktree(root, Path::new("/repo/../escape.rs")));
    }

    #[test]
    fn a_pending_delete_says_exactly_what_it_will_do() {
        let trash = PendingTreeDelete {
            path: PathBuf::from("/repo/a.rs"),
            is_dir: false,
            mechanism: DeleteMechanism::Trash {
                program: "gio",
                args: Vec::new(),
            },
        };
        assert_eq!(trash.confirm_label(), "Move to Trash");
        assert!(trash.explanation().contains("trash"));

        let permanent = PendingTreeDelete {
            path: PathBuf::from("/repo/src"),
            is_dir: true,
            mechanism: DeleteMechanism::Permanent,
        };
        assert_eq!(permanent.confirm_label(), "Delete permanently");
        assert!(
            permanent.explanation().contains("Permanently")
                && permanent
                    .explanation()
                    .contains("folder and everything in it"),
            "a permanent delete of a folder must say both of those things out loud: {}",
            permanent.explanation()
        );
        assert!(
            !permanent.explanation().to_lowercase().contains("trash to")
                && !permanent.explanation().starts_with("Move"),
            "the permanent branch must never claim a trash that doesn't happen"
        );
    }
}
