//! Real "New file" creation - the file tree's own always-visible per-directory and root-level
//! "+" affordances (`crate::sidebar::render`) funnel into this module. The tab-strip `+` menu had
//! its own "New file" row until Revision R12 §3 pared that menu down to the spec's five items
//! (*New terminal* / *New agent* / *Git graph* / *Open file…* / *Next changed file*, none of them
//! "New file"); the file tree's affordances were already a real, always-present second entry
//! point to this same flow, so removing the menu row dropped no reachable functionality.
//!
//! Naming UI: a small, hand-rolled append/backspace-only inline text field
//! (`Self::handle_new_file_key_down`), the same minimal shape `Self::handle_filter_key_down`
//! already established for the rail's filter row - there is no existing "rename"/"create" text
//! prompt anywhere else in this app to match instead (agents have no user-assigned names at
//! all), and a single-line name prompt doesn't warrant pulling in the full `EntityInputHandler`
//! machinery the File view's real text editing uses (`vendor/zed/crates/gpui/examples/input.rs`).

use super::*;
use std::path::Path;

/// State for an in-progress "New file" prompt - `Some` only while the inline name field is
/// showing. Opened by [`AdeApp::start_new_file`], closed by [`AdeApp::create_new_file`] (on
/// success) or [`AdeApp::cancel_new_file`] (Escape, or the scrim).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewFileInputState {
    /// The real directory the new file will be created in - the file tree's own root-level "+"
    /// (the worktree root) or the specific directory row whose own "+" was clicked.
    pub(super) parent_dir: PathBuf,
    /// The name typed so far - append/backspace only, mirroring [`AdeApp::filter_query`], with
    /// its own real undo history (GitHub issue #17). The history lives and dies with this prompt,
    /// which is exactly the per-widget lifetime that issue asks for.
    pub(super) name: text_history::TextField,
}

impl AdeApp {
    /// Opens the inline "New file" name prompt, scoped to `parent_dir`.
    pub(crate) fn start_new_file(
        &mut self,
        parent_dir: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_file_input = Some(NewFileInputState {
            parent_dir,
            name: text_history::TextField::new(),
        });
        self.new_file_error = None;
        window.focus(&self.new_file_focus_handle, cx);
        cx.notify();
    }

    /// Closes the prompt without creating anything - the Escape key, or a click outside it.
    pub(super) fn cancel_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_file_input = None;
        self.new_file_error = None;
        // Real, live-reproduced fix (found in this revision's own self-audit): move focus
        // somewhere still real and rendered rather than leaving it dangling on the now-hidden
        // prompt field (`Self::new_file_focus_handle`) - onto the code surface's own focus
        // target if a file tab is showing (an earlier version of this method only ever checked
        // `self.agents.focus_active`, which is a no-op while a file tab occupies the centre
        // pane, so cancelling "New file" while a file was open left focus dangling on the
        // now-unrendered prompt - the exact "focus left pointing at something no longer
        // rendered" class this project keeps re-finding), or the active agent's pane otherwise
        // (the same guard `Self::focus_newly_spawned_agent` uses). If neither is showing - no
        // file tab *and* no active agent, a real, reachable state under the tabs rework
        // (`Agents::active_id`'s own docs, and `Self::select_worktree`'s identical fallback) -
        // `Agents::focus_active` is a genuine no-op, so fall back to the rail's own filter
        // root container (`Self::rail_focus_handle`) the same way `Self::select_worktree` does -
        // deliberately the rail's root, not its filter field, which this used to target; see that
        // handle's own docs for the real keystroke-swallowing bug that was - rather
        // than leaving `Window::focus` dangling on the just-closed prompt field.
        if self.graph_tab_active {
            // The graph tab (like a file tab) occupies the centre pane instead of an agent's
            // pane - a real, adversarial-audit-found gap: falling through to
            // `agents.focus_active` here would focus a pane `Self::render_center_pane` isn't
            // drawing while the graph tab is showing.
            window.focus(&self.graph_focus_handle, cx);
        } else if self.open_change.is_some() {
            window.focus(&self.code_focus_handle, cx);
        } else if self.agents.active_id().is_some() {
            self.agents.focus_active(window, cx);
        } else {
            window.focus(&self.rail_focus_handle, cx);
        }
        cx.notify();
    }

    /// The inline name field's key handler - append/backspace/Enter (create)/Escape (cancel),
    /// mirroring [`Self::handle_filter_key_down`]'s own minimal shape and its same "leave
    /// modified keystrokes unhandled so app-level shortcuts still work" rule.
    pub(super) fn handle_new_file_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        match keystroke.key.as_str() {
            "escape" => {
                self.cancel_new_file(window, cx);
                cx.stop_propagation();
            }
            "enter" => {
                self.create_new_file(window, cx);
                cx.stop_propagation();
            }
            "backspace" => {
                if let Some(input) = self.new_file_input.as_mut() {
                    input.name.pop(Instant::now());
                    // GitHub issue #27's "solid mid-keystroke" - see `crate::rail::render::
                    // AdeApp::handle_filter_key_down`'s identical reasoning. Missing here
                    // (GitHub issue #45) is exactly why this field never blinked.
                    self.reset_caret_blink(cx);
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
                    if let Some(input) = self.new_file_input.as_mut() {
                        input.name.push_str(text, Instant::now());
                        self.reset_caret_blink(cx);
                        cx.notify();
                        cx.stop_propagation();
                    }
                }
            }
        }
    }

    /// `TextUndo`/`TextRedo` for the "New file" name prompt (GitHub issue #17). Clears the
    /// stale validation error alongside the text: a message like "file name can't contain a path
    /// separator" describes a name the user has just stepped away from.
    pub(super) fn handle_new_file_text_undo(
        &mut self,
        _: &TextUndo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .new_file_input
            .as_mut()
            .is_some_and(|input| input.name.undo())
        {
            self.new_file_error = None;
            cx.notify();
        }
    }

    pub(super) fn handle_new_file_text_redo(
        &mut self,
        _: &TextRedo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .new_file_input
            .as_mut()
            .is_some_and(|input| input.name.redo())
        {
            self.new_file_error = None;
            cx.notify();
        }
    }

    /// The inline "New file" prompt: a scrim + small centered panel, the same overlay shape
    /// [`Self::render_plus_menu`] uses (transparent scrim, panel stops the click from bubbling
    /// up and closing it) - centered on screen (rather than anchored to a button's painted
    /// bounds, like the `+` menu/palette are) since this prompt can be opened from two different
    /// places (the `+` menu row, and the file tree's own hover "+" affordance), each with a
    /// different real position - a fixed anchor would need to track whichever one most recently
    /// opened it. Assumes `Self::new_file_input` is `Some` - the caller
    /// (`crate::root::AdeApp::render`) only renders this when it is, mirroring
    /// `Self::render_plus_menu`'s identical assumption for `Self::plus_menu_open`.
    pub(super) fn render_new_file_prompt(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self
            .new_file_input
            .as_ref()
            .map(|input| input.name.as_str().to_string())
            .unwrap_or_default();
        let parent_label = self
            .new_file_input
            .as_ref()
            .map(|input| input.parent_dir.display().to_string())
            .unwrap_or_default();

        div()
            .id("new-file-scrim")
            .absolute()
            // Starts *below* the title bar, exactly like `crate::palette::render`'s own scrim
            // does and for the same reason - now a real one, since this layer `.occlude()`s.
            // A full-window occluding scrim swallows the window's own close/minimise/maximise
            // caption buttons and the title bar's drag region, so the window could not be closed
            // or moved while it was up. Reproduced against the real caption button by this
            // change's own adversarial audit.
            .top(theme::band::TITLE_BAR)
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
            .flex()
            .items_center()
            .justify_center()
            // The same real modal layer the file tree's delete confirmation is - a transparent-
            // to-the-mouse scrim would let the workspace behind it keep taking clicks and
            // painting hover states while a modal is up. See
            // `crate::sidebar::render::AdeApp::render_tree_context_menu`'s own docs for what
            // `.occlude()` actually does, and `crate::root::widgets::modal_scrim_bg` for why the
            // fill is a token rather than the raw `gpui::black()` that used to be here.
            .occlude()
            .bg(widgets::modal_scrim_bg())
            .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                this.cancel_new_file(window, cx);
            }))
            .child(
                div()
                    .id("new-file-panel")
                    .track_focus(&self.new_file_focus_handle)
                    // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why the
                    // tag and the listeners both live on this exact node.
                    .key_context("text-input")
                    .on_action(cx.listener(Self::handle_new_file_text_undo))
                    .on_action(cx.listener(Self::handle_new_file_text_redo))
                    .on_key_down(cx.listener(Self::handle_new_file_key_down))
                    .on_click(cx.listener(|_this, _event: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                    }))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .w(px(320.0))
                    .p(px(12.0))
                    .bg(theme::surface::PALETTE)
                    .border_1()
                    .border_color(theme::border::POPOVER)
                    .rounded(theme::radius::CARD)
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(px(11.5))
                            .text_color(theme::text::HEADING)
                            .child("New file"),
                    )
                    .child(
                        div()
                            .font(font(theme::font::MONO))
                            .text_size(px(9.5))
                            .text_color(theme::text::FAINTER)
                            .child(parent_label),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(5.0))
                            .rounded(theme::radius::CHIP)
                            .bg(theme::surface::SEGMENT_TRACK)
                            .flex()
                            .items_center()
                            // GitHub issue #45 / live report: this prompt had no caret element
                            // at all before this - just the typed name or its placeholder, no
                            // insertion-point indicator, and `new_file_focus_handle` was never
                            // wired into `crate::root::caret_blink` either (see
                            // `Self::new_with_settings`'s own comment on the fix). The caret and
                            // the text around it now come from the one helper that owns that
                            // structure - see `AdeApp::render_simple_input_row`.
                            .child(self.render_simple_input_row(widgets::SimpleInput {
                                caret_selector: "new-file-caret".into(),
                                text_selector: "new-file-name-text".into(),
                                focus_handle: Some(&self.new_file_focus_handle),
                                text: &name,
                                placeholder: "file-name.ext",
                                font: theme::font::MONO,
                                text_size: px(11.5),
                                // One colour for both: this prompt has never dimmed its
                                // placeholder, and the migration is not the place to change that.
                                text_color: theme::text::BODY,
                                placeholder_color: theme::text::BODY,
                            })),
                    )
                    .when_some(self.new_file_error.clone(), |el, error| {
                        el.child(
                            div()
                                .font(font(theme::font::SANS))
                                .text_size(px(10.5))
                                .text_color(theme::status::FAIL)
                                .child(error),
                        )
                    })
                    .child(
                        div()
                            .font(font(theme::font::SANS))
                            .text_size(px(10.0))
                            .text_color(theme::text::GHOST)
                            .child("enter to create \u{b7} esc to cancel"),
                    ),
            )
    }

    /// Creates the real, empty file the "New file" prompt named and opens it in the File view,
    /// the same real end state a Files-tree row click on an existing file reaches
    /// (`crate::code_surface::tabs::AdeApp::open_file_view`) - inlined here rather than reused
    /// as-is since there's no on-disk file yet for that method's own load path to read.
    ///
    /// Refuses - a real, visible error, with the prompt left open so the name can be corrected -
    /// rather than silently overwriting an existing file or directory already at the same path.
    /// The new [`edit_buffer::EditBuffer`] is seeded with `saved_mtime: None`/`saved_len: 0`:
    /// genuinely nothing has been written to disk yet. The very next step calls
    /// [`Self::save_active_file`] to perform that real first write, through the exact same
    /// freshness-gated save pipeline every other save uses - see that method's own
    /// `is_new_never_saved` docs for why a brand-new, never-loaded path doesn't trip its
    /// external-change-conflict check the way it otherwise would.
    pub(super) fn create_new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(input) = self.new_file_input.clone() else {
            return;
        };
        // `input.name` is a `text_history::TextField` (GitHub issue #17), not a `String`, so the
        // shared creation path is handed its `as_str()`. Validation - including the empty-name
        // and path-separator rules this method used to apply inline - lives in
        // `create_file_named`'s one shared `file_ops::validate_entry_name` call.
        if let Err(message) =
            self.create_file_named(&input.parent_dir, input.name.as_str(), window, cx)
        {
            self.new_file_error = Some(message);
            cx.notify();
        }
    }

    /// [`Self::create_new_file`]'s real body, callable without the modal prompt's own state - the
    /// file tree's inline "New file" editor (GitHub issue #19 §2) drives this directly, so both
    /// affordances create a file through one implementation rather than two.
    ///
    /// Returns the real rejection message on failure and leaves nothing changed, so each caller
    /// can surface it next to *its own* field. Clears [`AdeApp::new_file_input`] on success (the
    /// modal prompt's own dismissal) - a no-op for the tree's editor, which never opened it.
    pub(crate) fn create_file_named(
        &mut self,
        parent_dir: &Path,
        raw_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        // The one shared validator (`crate::sidebar::file_ops::validate_entry_name`), not this
        // method's own inline copy of the rules any more: the file tree's inline New File /
        // New Folder / Rename editors (GitHub issue #19 §2) apply exactly the same ones, and two
        // hand-maintained copies is how they drift into disagreeing about what a legal name is.
        let name = crate::sidebar::file_ops::validate_entry_name(raw_name)?;

        let absolute_path = parent_dir.join(name);
        // `symlink_metadata`, not `exists()`: a *broken* symlink is a real directory entry that
        // the create below would collide with, and `exists()` follows the link and reports
        // `false` for it (the same reasoning `file_ops::unique_destination` documents).
        if absolute_path.symlink_metadata().is_ok() {
            return Err(format!("\"{name}\" already exists"));
        }

        let relative = absolute_path
            .strip_prefix(&self.file_tree_root)
            .map(|stripped| stripped.to_path_buf())
            .unwrap_or_else(|_| absolute_path.clone());
        let extension = absolute_path
            .extension()
            .map(|ext| ext.to_string_lossy().into_owned());

        self.new_file_input = None;
        self.new_file_error = None;
        self.prune_confirm_armed = false;
        self.discard_confirm_armed = None;
        // A second real "open a file" path that doesn't go through
        // `crate::code_surface::tabs::AdeApp::open_and_focus_file` - a real, adversarial-audit-
        // found gap: without this, creating a file while the graph tab was active left it
        // `graph_tab_active` (so `Self::render_center_pane` kept showing the graph, not this new
        // file) with `Window::focus` on `code_focus_handle`, which isn't in that frame either.
        self.leave_graph_tab(window, cx);
        // GitHub issue #225: the review tab occupies the centre pane exactly as the graph tab
        // does, so it needs the identical teardown here. Without this, `review_tab_active` stayed
        // set, `render_center_pane` kept returning the review body, and the tab this call is
        // switching *to* never mounted at all - while real focus had already moved onto it. Found
        // by an adversarial audit; the review surface's own docs claimed to copy the graph tab's
        // discipline and, in exactly this way, did not.
        self.leave_review_tab(window, cx);

        self.focus_code_surface(window, cx);
        self.pending_cursor_line = None;
        if !self
            .open_files()
            .iter()
            .any(|open| open.as_path() == relative.as_path())
        {
            self.open_files_mut().push(relative.clone());
        }
        self.open_change = Some(relative.clone());
        self.code_view = code_view::CodeView::File;
        self.markdown_view = markdown_preview::MarkdownView::Source;
        self.selected_tree_path = Some(absolute_path.clone());
        // Now that the tree starts collapsed (GitHub issue #18 §1), a file created inside a
        // folder nobody has expanded yet would otherwise be highlighted on a row that isn't
        // showing at all. Same real reveal - and same recorded expansions - as the palette's own
        // "reveal in tree".
        self.reveal_in_tree(&absolute_path, cx);
        let options = self.highlight_options();
        self.insert_edit_buffer(
            relative,
            edit_buffer::EditBuffer::new_with_options(
                absolute_path,
                String::new(),
                extension,
                None,
                0,
                options,
            ),
        );
        self.refresh_open_diff_file_cache();
        self.dismiss_hover();
        self.dismiss_completions();
        self.save_active_file(cx);
        self.load_file_tree(self.file_tree_root.clone(), cx);
        // A just-created file gets a real tab here, without going through
        // `crate::code_surface::tabs::AdeApp::open_and_focus_file` (this method does that work
        // itself, plus a seeded empty edit buffer) - so it needs its own recording call, or the
        // tab a user just made would be the one kind that didn't survive a relaunch. See
        // `crate::work_surface::session`.
        self.record_worktree_session(cx);
        cx.notify();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::time::Instant;

    /// Real, live-reproduced regression coverage for the self-audit finding: cancelling the
    /// "New file" prompt while a file tab is showing must not leave `Window::focus` dangling on
    /// the now-hidden prompt field - proven the same way this project's other dangling-focus
    /// fixes are (`root::focus::tab_strip_keybinding_tests`' own precedent): a real ⌘P keystroke
    /// afterward must still reach `TogglePalette`.
    #[gpui::test]
    fn cancelling_new_file_while_a_file_tab_is_open_does_not_leave_focus_dangling(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("a.txt"), "hello\n").expect("write a.txt");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("a.txt"), window, cx);
            app.start_new_file(repo.path().to_path_buf(), window, cx);
            app.cancel_new_file(window, cx);
        });

        let key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(key);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after cancelling \"New file\" with a file tab still open \
             must still open the palette - before the fix, focus was left dangling on the \
             now-hidden prompt field"
        );
    }

    /// Real, live-reproduced coverage for the case [`super::AdeApp::cancel_new_file`] didn't
    /// handle before this revision's own self-audit: no file tab open *and* no active agent at
    /// all (every agent in the worktree already closed) - a real, reachable state under the
    /// tabs rework (`crate::work_surface::agents::Agents::active_id`'s own docs, and
    /// `crate::root::AdeApp::select_worktree`'s identical fallback for the same case). Before the
    /// fix, `Agents::focus_active` was a genuine no-op here, leaving `Window::focus` dangling
    /// on the just-closed prompt field.
    #[gpui::test]
    fn cancelling_new_file_with_no_file_tab_and_no_active_agent_does_not_leave_focus_dangling(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));

        let initial_id = app.read_with(cx, |app, _| {
            app.agents.active_id().expect("initial shell agent")
        });
        app.update_in(cx, |app, window, cx| {
            app.close_agent(initial_id, window, cx);
        });
        assert!(
            app.read_with(cx, |app, _| app.agents.active_id().is_none()),
            "sanity check: closing the only agent should leave none active"
        );

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
            app.cancel_new_file(window, cx);
        });

        let key = if cfg!(target_os = "macos") {
            "cmd-p"
        } else {
            "ctrl-p"
        };
        cx.simulate_keystrokes(key);
        assert!(
            app.read_with(cx, |app, _| app.palette_open),
            "a real {key} keystroke after cancelling \"New file\" with no file tab and no \
             active agent must still open the palette - before the fix, \
             Agents::focus_active was a no-op here and Window::focus was left dangling on the \
             now-hidden prompt field"
        );
    }

    /// End-to-end against a real temp directory: naming a file and pressing Enter must produce
    /// a real, empty file on disk - not just an in-memory tab - and must not trip the
    /// external-change-conflict path `Self::save_active_file`'s freshness gate exists for.
    #[gpui::test]
    fn creating_a_new_file_writes_a_real_empty_file_with_no_false_conflict(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
            app.new_file_input
                .as_mut()
                .expect("prompt should be open")
                .name
                .set("notes.md", Instant::now());
            app.create_new_file(window, cx);
        });
        cx.run_until_parked();

        let real_path = repo.path().join("notes.md");
        assert!(
            real_path.exists(),
            "creating a new file must write a real file to disk, not just open an in-memory tab"
        );
        assert_eq!(
            std::fs::read_to_string(&real_path).expect("read"),
            "",
            "a freshly created file should be empty"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.file_save_error.is_none(),
                "the new-file flow must not hit the false \"changed on disk since it was \
                 opened\" conflict for a path that never had anything on disk in the first \
                 place - got: {:?}",
                app.file_save_error
            );
            assert!(
                !app.file_external_conflict
                    .contains(std::path::Path::new("notes.md")),
                "a brand-new path must never be flagged as an external conflict"
            );
            assert_eq!(
                app.open_change.as_deref(),
                Some(std::path::Path::new("notes.md"))
            );
        });
    }

    #[gpui::test]
    fn creating_a_file_that_already_exists_is_refused_with_a_real_error(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("existing.txt"), "already here").expect("seed file");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
            app.new_file_input
                .as_mut()
                .expect("prompt should be open")
                .name
                .set("existing.txt", Instant::now());
            app.create_new_file(window, cx);
        });
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(repo.path().join("existing.txt")).expect("read"),
            "already here",
            "an existing file's real content must never be silently overwritten"
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.new_file_error.is_some(),
                "attempting to create an already-existing file must surface a real error"
            );
            assert!(
                app.new_file_input.is_some(),
                "the prompt should stay open so the name can be corrected"
            );
        });
    }

    #[gpui::test]
    fn escape_cancels_the_new_file_prompt_without_touching_disk(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
            app.new_file_input
                .as_mut()
                .expect("prompt should be open")
                .name
                .set("abandoned.rs", Instant::now());
            app.cancel_new_file(window, cx);
        });
        cx.run_until_parked();

        assert!(!repo.path().join("abandoned.rs").exists());
        app.read_with(cx, |app, _| {
            assert!(app.new_file_input.is_none());
        });
    }
}

/// GitHub issue #45 ("Input blink only on focused input or file") plus a live follow-up report:
/// this prompt had no caret element at all - confirmed by reading `render_new_file_prompt`
/// before this fix, just the typed name or its `"file-name.ext"` placeholder - and
/// `new_file_focus_handle` was never threaded through `AdeApp::wire_caret_blink` either. Real
/// interaction coverage, mirroring `crate::rail::render::rail_filter_caret_tests`' own
/// measured-bounds technique.
#[cfg(test)]
mod new_file_caret_tests {
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;

    #[gpui::test]
    fn caret_sits_before_the_placeholder_when_empty_and_after_the_text_once_typed(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
        });
        cx.run_until_parked();

        let empty_caret = cx.debug_bounds("new-file-caret").expect(
            "the caret should have really painted with an empty name - before this fix, no \
             caret element existed here at all, so this selector would never resolve",
        );
        let placeholder = cx
            .debug_bounds("new-file-name-text")
            .expect("the placeholder text should have really painted");
        assert!(
            empty_caret.origin.x <= placeholder.origin.x,
            "with an empty name, the real caret must sit before (at or left of) the \
             placeholder's own start x, not after it - got caret {:?} vs placeholder {:?}",
            empty_caret,
            placeholder,
        );

        cx.simulate_input("main.rs");
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .new_file_input
                .as_ref()
                .expect("still open")
                .name
                .as_str()
                .to_string()),
            "main.rs",
            "sanity check: real typed name"
        );

        let typed_caret = cx
            .debug_bounds("new-file-caret")
            .expect("the caret should have really painted with a typed name");
        let typed_text = cx
            .debug_bounds("new-file-name-text")
            .expect("the real typed name should have really painted");
        assert!(
            typed_caret.origin.x >= typed_text.origin.x + typed_text.size.width,
            "with a typed name, the real caret must sit at or after the typed text's own right \
             edge, not before it - got caret {:?} vs text {:?}",
            typed_caret,
            typed_text,
        );
        assert!(
            typed_caret.origin.x > empty_caret.origin.x,
            "the caret's real measured horizontal position must differ between the empty-name \
             state (before the placeholder) and a typed-name state (after the real text) - got \
             {:?} vs {:?}",
            empty_caret.origin.x,
            typed_caret.origin.x,
        );
    }

    /// GitHub issue #45's own title, taken literally: before this fix, `new_file_focus_handle`
    /// was never wired into `AdeApp::wire_caret_blink` at all, so *blurring* it never called the
    /// real [`crate::root::AdeApp::stop_caret_blink`] that pins the shared flag straight to
    /// "dimmed" the instant focus leaves.
    ///
    /// Checks *blur*, not *focus* - see
    /// `crate::graph_view::render::graph_focus_tests::blurring_the_branches_filter_stops_the_real_shared_blink_loop`'s
    /// own docs for why a focus-then-type test can't actually distinguish "wired into
    /// `wire_caret_blink`" from "`Self::handle_new_file_key_down`'s own real `reset_caret_blink`
    /// call on every keystroke, regardless of that wiring" - only blur has an effect that's
    /// unique to the real subscription this fix adds.
    #[gpui::test]
    fn blurring_the_new_file_prompt_stops_the_real_shared_blink_loop(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        // `on_focus`/`on_blur` listeners (`AdeApp::wire_caret_blink`'s own mechanism) only
        // fire while GPUI considers the window itself "active" - a real, freshly opened test
        // window starts out not active at all, so this is a real, necessary precondition, not
        // test scaffolding for its own sake.
        app.update_in(cx, |_app, window, _cx| window.activate_window());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.start_new_file(repo.path().to_path_buf(), window, cx);
        });
        cx.simulate_input("m");
        assert!(
            app.read_with(cx, |app, _| app.caret_blink_visible),
            "sanity check: typing must leave the caret solid/visible, whether that came from \
             this fix's own on-focus wiring or `handle_new_file_key_down`'s pre-existing \
             `reset_caret_blink` call"
        );

        // Move real keyboard focus to a genuinely different, real, *unwired* handle
        // (`AdeApp::rail_focus_handle`, the rail's own always-present container fallback, never
        // itself a caret-bearing input), then force the same real redraw a live blur event
        // needs to be observed in this test harness, via a harmless, unbound plain keystroke.
        app.update_in(cx, |app, window, cx| {
            window.focus(&app.rail_focus_handle, cx);
        });
        cx.simulate_keystrokes("x");

        assert!(
            !app.read_with(cx, |app, _| app.caret_blink_visible),
            "blurring the \"New file\" prompt must have immediately pinned the shared caret-blink \
             flag off (`AdeApp::stop_caret_blink`) - before this fix, \
             `new_file_focus_handle` was never threaded through `AdeApp::wire_caret_blink`, so \
             nothing would have reacted to it losing focus at all, and the caret would have \
             stayed solid/visible until whatever timer the earlier keystroke started eventually \
             fired on its own"
        );
    }
}
