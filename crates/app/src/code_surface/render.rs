//! Surface C's shell: which of the two views (Diff or File) an active tab actually shows,
//! the key context that decides which keybindings are live over it, and the segmented
//! Diff/File toggle that switches between them.

use super::diff_view::render_accept_file_button;
use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::{render_sidebar_message, render_tag_pill};
use crate::settings::widgets::ChoiceOption;

impl AdeApp {
    /// A themed explanatory message for every [`DiffLoadState`] that isn't a loaded diff.
    pub(crate) fn render_diff_state_message(&self) -> gpui::AnyElement {
        let (text, color) = match &self.diff_state {
            DiffLoadState::Loading => ("computing diff...".to_string(), theme::text::FAINT),
            DiffLoadState::Error(err) => (
                format!("failed to compute diff: {err}"),
                theme::status::FAIL,
            ),
            DiffLoadState::Loaded(DiffBase::NoBaseFound) => (
                "no base branch could be detected for this worktree, and HEAD is unborn (no \
                 commits yet), so there is nothing to diff at all"
                    .to_string(),
                theme::text::FAINT,
            ),
            // Unreachable in practice (callers check `current_diff()` first, which is `Some`
            // for both real variants below via `DiffBase::diff()` - GitHub issue #108's
            // uncommitted-vs-HEAD fallback means `NoBase` is never actually "nothing to show").
            // Matched explicitly so a future `DiffBase` variant isn't silently swallowed by a
            // wildcard.
            DiffLoadState::Loaded(DiffBase::Diff(_) | DiffBase::NoBase { .. }) => {
                (String::new(), theme::text::FAINT)
            }
        };
        render_sidebar_message(text, color.into())
    }

    /// The centre's single-file Surface C, opened by a Changes-row click (`diff_file` always
    /// `Some`) or a Files-tree row click (`diff_file` may be `None`): a toolbar (dir/name, tag
    /// pill, +n/-n stats, the `Diff | File` toggle, the zoom group, `Accept file`, close) over
    /// either [`Self::render_diff_file_detail`]'s folded hunk content or [`Self::render_file_view`]'s
    /// syntax-highlighted content, both zoom-scoped through [`zoom_scoped`].
    ///
    /// `effective_view` forces `File` when `diff_file` is `None`, regardless of what
    /// `self.code_view` was last left at by a different file.
    pub(crate) fn render_code_surface(
        &mut self,
        relative_path: &Path,
        diff_file: Option<&DiffFile>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (dir, name) = changes::split_dir_name(relative_path);
        let tag = diff_file.and_then(|file| changes::change_tag(file.status));
        let stats = diff_file.map(changes::diff_file_stats);
        let rename_label = diff_file.and_then(changes::rename_label);
        let has_diff = diff_file.is_some();
        let effective_view = if has_diff {
            self.code_view
        } else {
            code_view::CodeView::File
        };

        let toolbar = div()
            .flex_none()
            .h(theme::band::DIFF_TOOLBAR)
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .bg(theme::surface::HEADER)
            .border_b_1()
            .border_color(theme::border::INNER)
            .when(!dir.is_empty(), |el| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::text::GHOST)
                        .child(format!("{dir}/")),
                )
            })
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(px(11.5))
                    .text_color(theme::text::HEADING)
                    .child(name),
            )
            .when_some(tag, |el, tag| el.child(render_tag_pill(tag)))
            .when_some(stats, |el, (add, del)| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::diff::STAT_ADD)
                        .child(format!("+{add}")),
                )
                .child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.5))
                        .text_color(theme::diff::STAT_DEL)
                        .child(format!("\u{2212}{del}")),
                )
            })
            // The row's compact `render_moved_tag` has no room for the pre-rename path; the
            // toolbar does.
            .when_some(rename_label, |el, label| {
                el.child(
                    div()
                        .font(font(theme::font::MONO))
                        .text_size(px(10.0))
                        .text_color(theme::text::GHOST)
                        .child(label),
                )
            })
            .child(div().flex_1())
            .child(self.render_diff_file_toggle(has_diff, effective_view, cx))
            .child(self.render_zoom_control(cx))
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(render_accept_file_button(
                self.window_controls_style().is_macos(),
            ))
            .child(
                div()
                    .id("close-diff-surface")
                    .cursor_pointer()
                    .font(font(theme::font::MONO))
                    .text_size(px(11.0))
                    .text_color(theme::text::GHOST)
                    .hover(|el| el.text_color(theme::text::PRIMARY))
                    .child("\u{d7} close")
                    .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
                        this.close_change_diff(window, cx);
                    })),
            );

        let body = match (effective_view, diff_file) {
            (code_view::CodeView::Diff, Some(file)) => self.render_diff_file_detail(file, cx),
            _ => self.render_file_view(relative_path, cx),
        };

        // Whether the real, editable File view (Revision R8.5a) - not the read-only Diff view -
        // is showing right now, with a real `EditBuffer` actually backing it.
        //
        // `"file-editor"` is added to the *same* node's key context as the pre-existing
        // `"diff"` one (space-separated - `gpui::KeyContext` treats a context string as a real
        // set of identifiers, matched independently, not a single opaque token; verified against
        // `vendor/zed/crates/gpui/src/keymap/context.rs`), rather than on a separate inner
        // container the way an earlier version of this code tried: GPUI's real key dispatch
        // builds its context stack (and bubbles `on_action`) from the *focused* node up through
        // its ancestors only (`vendor/zed/crates/gpui/src/window.rs::dispatch_key_event`, via
        // `focus_node_id_in_rendered_frame`/`dispatch_path`) - a context or `on_action` set on a
        // *descendant* of the focused node (which `code_focus_handle`'s own `track_focus` below
        // already pins to *this* outer div) is never reachable from a real dispatch, a real,
        // live-verified bug an earlier version of this code shipped with (real keystroke
        // simulation tests against `EditorLeft`/`EditorSave`/etc. failed until this moved here).
        // The Diff view still genuinely never receives these bindings: `is_file_editor` is false
        // whenever `effective_view` isn't `File`, so the context string never gains
        // `"file-editor"` in that case.
        let is_file_editor =
            effective_view == code_view::CodeView::File && self.edit_buffer_contains(relative_path);
        // Real Completions popup context (Revision R8.5b) - added the same way `"file-editor"`
        // itself is, only while a popup is genuinely, *actionably* open *for this exact file*
        // (matching `Self::completions_open_for_active_path`'s own guard, though that reads the
        // active tab rather than `relative_path` directly - both agree here, since this whole
        // surface only ever renders for whichever path is actually active). "Actionably" is
        // load-bearing, not decoration: `completions_open_for_active_path` only returns `true`
        // for a genuine `CompletionsStatus::Ready` entry, never a merely-`Loading`/`Failed` one
        // (Revision R8.5b audit finding 1's fix for a real, live-reproduced bug - see that
        // method's own docs) - so `Enter`/`Up`/`Down` fall back to the plain `Editor*` bindings
        // below for the entire real round-trip a completion request takes, not just once it
        // resolves. `crate::default_key_bindings` scopes `CompletionsUp`/`CompletionsDown`/
        // `CompletionsAccept`/`CompletionsDismiss` to `Some("file-editor && completions")` and
        // correspondingly narrows the plain `Editor*` up/down/enter bindings to
        // `Some("file-editor && !completions")` - see those bindings' own docs for why this is
        // the same real `&&`/`!` predicate mechanism the `"]"` binding already established, not a
        // new one.
        let completions_open = is_file_editor && self.completions_open_for_active_path();
        // `"text-input"` (GitHub issue #17) rides alongside `"file-editor"` on exactly the same
        // node, for exactly the same real reason `"file-editor"` itself does (see above): it is
        // the one shared tag every real text-typing surface in this app carries, and it is what
        // routes `secondary-z` to text undo. See `crate::default_key_bindings`' own docs for the
        // full rationale. It is added only in the `is_file_editor` cases: the read-only Diff view
        // has no text history to undo.
        let key_context = match (is_file_editor, completions_open) {
            (true, true) => "diff file-editor text-input completions",
            (true, false) => "diff file-editor text-input",
            (false, _) => "diff",
        };

        div()
            .id("code-surface")
            // Focus target for the whole Diff/File surface - see `code_focus_handle`'s docs for
            // the dangling-`Window::focus` bug this fixes, the same class `render_settings`'s
            // identical `track_focus` fixes for the Settings surface.
            .track_focus(&self.code_focus_handle)
            // Scopes `]` (`NextChangedFile`) to only fire while a file tab has focus - see that
            // binding's docs for the terminal-input-swallowing bug this prevents. `"file-editor"`
            // (Revision R8.5a's real File view text editing) is added the same way - see this
            // method's own docs, above, for why both live on this one node.
            .key_context(key_context)
            // Harmless when `key_context` doesn't include `"file-editor"` (the Diff view, or a
            // File view with no buffer yet): none of `crate::default_key_bindings`' real
            // `"file-editor"`-scoped bindings can ever be found in that case, and every handler
            // below is independently guarded by `AdeApp::active_editable_path` regardless.
            .on_action(cx.listener(Self::handle_editor_backspace_action))
            .on_action(cx.listener(Self::handle_editor_delete_action))
            .on_action(cx.listener(Self::handle_editor_enter_action))
            .on_action(cx.listener(Self::handle_editor_left_action))
            .on_action(cx.listener(Self::handle_editor_right_action))
            .on_action(cx.listener(Self::handle_editor_up_action))
            .on_action(cx.listener(Self::handle_editor_down_action))
            .on_action(cx.listener(Self::handle_editor_select_left_action))
            .on_action(cx.listener(Self::handle_editor_select_right_action))
            .on_action(cx.listener(Self::handle_editor_select_up_action))
            .on_action(cx.listener(Self::handle_editor_select_down_action))
            .on_action(cx.listener(Self::handle_editor_word_left_action))
            .on_action(cx.listener(Self::handle_editor_word_right_action))
            .on_action(cx.listener(Self::handle_editor_select_word_left_action))
            .on_action(cx.listener(Self::handle_editor_select_word_right_action))
            .on_action(cx.listener(Self::handle_editor_home_action))
            .on_action(cx.listener(Self::handle_editor_end_action))
            .on_action(cx.listener(Self::handle_editor_select_all_action))
            // Multi-cursor (Revision R13, issue #28) - `crate::code_surface::edit_buffer`'s own
            // "Multi-cursor" docs for the overall design. File-view-only, like every other
            // `Editor*` binding above: `crate::merge::editing`'s own `"merge-editor"` context
            // deliberately does not register these - a real, documented scope narrowing (the
            // merge hand-edit surface is secondary and less-used), not an oversight, and the
            // underlying `EditBuffer::secondary_cursors` field simply stays empty forever for a
            // merge-edit buffer as a result, leaving its own single-cursor behavior provably
            // unaffected.
            .on_action(cx.listener(Self::handle_editor_select_next_occurrence_action))
            .on_action(cx.listener(Self::handle_editor_select_all_occurrences_action))
            .on_action(cx.listener(Self::handle_editor_skip_occurrence_action))
            .on_action(cx.listener(Self::handle_editor_collapse_cursors_action))
            .on_action(cx.listener(Self::handle_editor_copy_action))
            .on_action(cx.listener(Self::handle_editor_cut_action))
            .on_action(cx.listener(Self::handle_editor_paste_action))
            .on_action(cx.listener(Self::handle_editor_save_action))
            .on_action(cx.listener(Self::handle_editor_save_anyway_action))
            .on_action(cx.listener(Self::handle_text_undo_action))
            .on_action(cx.listener(Self::handle_text_redo_action))
            .on_action(cx.listener(Self::handle_completions_up_action))
            .on_action(cx.listener(Self::handle_completions_down_action))
            .on_action(cx.listener(Self::handle_completions_accept_action))
            .on_action(cx.listener(Self::handle_completions_dismiss_action))
            .on_action(cx.listener(Self::handle_completions_invoke_action))
            .on_action(cx.listener(Self::handle_editor_indent_action))
            .on_action(cx.listener(Self::handle_editor_dedent_action))
            .on_action(cx.listener(Self::handle_editor_escape_action))
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(theme::surface::CENTER)
            .child(toolbar)
            .child(body)
            .into_any_element()
    }

    /// The toolbar's segmented `Diff | File` toggle. `Diff` is only clickable when `has_diff` is
    /// true ([`ChoiceOption::enabled_if`] disables it otherwise); `File` is always clickable.
    /// Shares [`Self::render_choice_control`] with the other segmented toggles in this file.
    pub(in crate::code_surface) fn render_diff_file_toggle(
        &self,
        has_diff: bool,
        effective_view: code_view::CodeView,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = match effective_view {
            code_view::CodeView::Diff => "Diff",
            code_view::CodeView::File => "File",
        };
        self.render_choice_control(
            "diff-file-toggle",
            &[
                ChoiceOption::enabled_if("Diff", has_diff),
                ChoiceOption::new("File"),
            ],
            selected.to_string(),
            cx,
            |this, index, _window, cx| {
                // Index 0 is `Diff`, index 1 is `File`, per the options array above.
                this.code_view = match index {
                    0 => code_view::CodeView::Diff,
                    _ => code_view::CodeView::File,
                };
                cx.notify();
            },
        )
    }
}

/// Proves the segmented `Diff | File` toggle's dispatch (`render_diff_file_toggle`, via the
/// shared `render_choice_control`) is driven by each segment's structural position, not its
/// display label - the R5.5 audit found the prior label-string dispatch could silently select
/// the wrong value if a label was renamed without updating `on_select`, with no compile error or
/// test failure. Clicks each segment by its structural `debug_selector` (never derived from the
/// label text) and asserts `code_view` matches that segment's position.
#[cfg(test)]
mod choice_control_dispatch_tests {
    use super::*;
    use gpui::TestAppContext;

    fn git_repo(dir: &std::path::Path, args: &[&str]) {
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
    fn clicking_a_segment_by_structural_position_selects_the_matching_real_value(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        git_repo(repo.path(), &["init", "-b", "main"]);
        git_repo(repo.path(), &["config", "user.email", "test@example.com"]);
        git_repo(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("a.txt"), "1\n").expect("write a.txt");
        git_repo(repo.path(), &["add", "."]);
        git_repo(repo.path(), &["commit", "-m", "initial"]);
        git_repo(repo.path(), &["checkout", "-b", "feature"]);
        std::fs::write(repo.path().join("a.txt"), "1\nchanged\n").expect("rewrite a.txt");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_change_diff(PathBuf::from("a.txt"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "sanity: opening a changed file's diff lands in Diff view by default"
        );

        // Segment at structural index 1 ("File") - clicked by its position-based selector,
        // never by searching for its label text.
        let file_bounds = cx
            .debug_bounds("choice-diff-file-toggle-1")
            .expect("the File segment must have painted at least once");
        cx.simulate_click(file_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File,
            "clicking the segment at structural index 1 must select File - position-based \
             dispatch, not a re-match on whatever that segment's label currently says"
        );

        // Segment at structural index 0 ("Diff") - back the other way, same mechanism.
        let diff_bounds = cx
            .debug_bounds("choice-diff-file-toggle-0")
            .expect("the Diff segment must have painted at least once");
        cx.simulate_click(diff_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "clicking the segment at structural index 0 must select Diff"
        );
    }
}
