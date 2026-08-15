//! Surface C's shell: which of the two views (Diff or File) an active tab actually shows,
//! the key context that decides which keybindings are live over it, and the segmented
//! File/Diff toggle that switches between them.

use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;
use crate::root::widgets::render_tag_pill;
use crate::settings::widgets::ChoiceOption;

impl AdeApp {
    /// A themed explanatory message for every [`DiffLoadState`] that isn't a loaded diff, as its
    /// own text plus the emphasis it deserves - so a caller that needs the words in a different
    /// container (the Changes panel's Against-main section states them as one of its own section
    /// notes, `crate::sidebar::sections::SectionRow::Note`) gets them from here rather than
    /// writing a second, drifting copy of the same four cases.
    pub(crate) fn diff_state_message(&self) -> (String, crate::theme::ColorToken) {
        match &self.diff_state {
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
        }
    }

    /// The centre's single-file Surface C, opened by a Changes-row click (`diff_file` always
    /// `Some`) or a Files-tree row click (`diff_file` may be `None`): a toolbar (dir/name, tag
    /// pill, +n/-n stats, the `File | Diff` toggle, the zoom group, close) over
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
        // GitHub issue #115: the `Source | Preview` toggle only makes sense for a real `.md` file
        // actually showing the File view - a diff hunk of a markdown file still shows its own
        // real diff, never a preview of one side of it.
        let is_markdown_file = effective_view == code_view::CodeView::File
            && relative_path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));

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
            .when(is_markdown_file, |el| {
                el.child(self.render_markdown_view_toggle(cx))
            })
            .child(self.render_diff_file_toggle(has_diff, effective_view, cx))
            .child(self.render_zoom_control(cx))
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(16.0))
                    .bg(theme::border::DIVIDER),
            )
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
            (code_view::CodeView::Diff, Some(file)) => {
                self.render_diff_file_detail(file, diff_view::DiffDetailSurface::Changes, cx)
            }
            _ if is_markdown_file
                && self.markdown_view == markdown_preview::MarkdownView::Preview =>
            {
                // `render_file_view` still runs first, purely for its real loading side effect
                // (`Self::spawn_file_load` on a stale/missing cache) - the same freshness dance
                // Source mode always does. Its returned element is only used as the fallback for
                // whichever frame the content genuinely isn't loaded/cached yet (a real "loading…"
                // or error message), never shown alongside a real preview.
                let fallback = self.render_file_view(relative_path, cx);
                match self.markdown_preview_source(relative_path) {
                    Some(source) => self.render_markdown_preview(&source, cx),
                    None => fallback,
                }
            }
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

    /// The toolbar's segmented `File | Diff` toggle. `Diff` is only clickable when `has_diff` is
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
                // GitHub issue #153: `File` first, `Diff` second - the order the segments
                // themselves paint in, matching the issue's own reference screenshot.
                ChoiceOption::new("File"),
                ChoiceOption::enabled_if("Diff", has_diff),
            ],
            selected.to_string(),
            cx,
            |this, index, _window, cx| {
                // Index 0 is `File`, index 1 is `Diff`, per the options array above.
                this.code_view = match index {
                    0 => code_view::CodeView::File,
                    _ => code_view::CodeView::Diff,
                };
                cx.notify();
            },
        )
    }

    /// GitHub issue #115's `Source | Preview` toggle for a `.md` file's File view - only rendered
    /// by [`Self::render_code_surface`] when [`code_view::CodeView::File`] is showing a real
    /// `.md` path. Shares [`Self::render_choice_control`] exactly like
    /// [`Self::render_diff_file_toggle`] does.
    pub(in crate::code_surface) fn render_markdown_view_toggle(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = match self.markdown_view {
            markdown_preview::MarkdownView::Source => "Source",
            markdown_preview::MarkdownView::Preview => "Preview",
        };
        self.render_choice_control(
            "markdown-view-toggle",
            &[ChoiceOption::new("Source"), ChoiceOption::new("Preview")],
            selected.to_string(),
            cx,
            |this, index, _window, cx| {
                // Index 0 is `Source`, index 1 is `Preview`, per the options array above.
                this.markdown_view = match index {
                    0 => markdown_preview::MarkdownView::Source,
                    _ => markdown_preview::MarkdownView::Preview,
                };
                cx.notify();
            },
        )
    }

    /// The real, already-loaded raw text `Self::render_markdown_preview` parses - preferring a
    /// live [`Self::edit_buffer`] (so a preview reflects unsaved edits, not just what's on disk)
    /// and falling back to the read-only [`Self::file_view_cache`] the same way
    /// [`Self::render_file_view`]'s own diagnostics indexing already does (see that call site's
    /// own docs) for a file with no edit buffer yet - truncated, non-UTF-8, or simply not loaded
    /// on this exact frame. `None` means "not ready yet", the caller's cue to show
    /// [`Self::render_file_view`]'s own loading/error state instead.
    fn markdown_preview_source(&self, relative_path: &Path) -> Option<String> {
        if let Some(buffer) = self.edit_buffer(relative_path) {
            return Some(buffer.content.clone());
        }
        let absolute_path = self.file_tree_root.join(relative_path);
        self.file_view_cache.as_ref().and_then(|parsed| {
            (parsed.path == absolute_path).then(|| {
                parsed
                    .lines
                    .iter()
                    .map(|line| line.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        })
    }
}

/// Proves the segmented `File | Diff` toggle's dispatch (`render_diff_file_toggle`, via the
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

        // Segment at structural index 0 ("File") - clicked by its position-based selector,
        // never by searching for its label text.
        let file_bounds = cx
            .debug_bounds("choice-diff-file-toggle-0")
            .expect("the File segment must have painted at least once");
        cx.simulate_click(file_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::File,
            "clicking the segment at structural index 0 must select File - position-based \
             dispatch, not a re-match on whatever that segment's label currently says"
        );

        // Segment at structural index 1 ("Diff") - back the other way, same mechanism.
        let diff_bounds = cx
            .debug_bounds("choice-diff-file-toggle-1")
            .expect("the Diff segment must have painted at least once");
        cx.simulate_click(diff_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.code_view),
            code_view::CodeView::Diff,
            "clicking the segment at structural index 1 must select Diff"
        );
    }
}

/// GitHub issue #115: the `Source | Preview` toggle only appears for a real `.md` file, and
/// switching to `Preview` renders the real parsed markdown tree (`markdown_preview`) with no
/// panic - end-to-end coverage on top of `markdown_preview::parse_tests`' own pure parser
/// coverage, matching `choice_control_dispatch_tests`' own real-window dispatch discipline.
#[cfg(test)]
mod markdown_preview_toggle_tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn the_toggle_only_appears_for_a_real_md_file(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("readme.md"), "# hi\n").expect("write readme.md");
        std::fs::write(repo.path().join("main.rs"), "fn main() {}\n").expect("write main.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("main.rs"), window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("choice-markdown-view-toggle-0").is_none(),
            "a non-markdown file must never show the Source/Preview toggle"
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("readme.md"), window, cx);
        });
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("choice-markdown-view-toggle-0").is_some(),
            "a real .md file's File view must show the Source/Preview toggle"
        );
    }

    #[gpui::test]
    fn clicking_preview_renders_the_real_parsed_document_with_no_panic(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            repo.path().join("readme.md"),
            "# Title\n\nSome **bold** text, a [link](https://example.com), and:\n\n- one\n- two\n\n\
             ```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n",
        )
        .expect("write readme.md");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("readme.md"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.markdown_view),
            markdown_preview::MarkdownView::Source,
            "sanity: a freshly opened file starts in Source view"
        );

        let preview_bounds = cx
            .debug_bounds("choice-markdown-view-toggle-1")
            .expect("the Preview segment must have painted");
        cx.simulate_click(preview_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.markdown_view),
            markdown_preview::MarkdownView::Preview
        );
        assert!(
            cx.debug_bounds("markdown-preview-content").is_some(),
            "switching to Preview must really mount the preview's own content container"
        );

        // Back to Source - the real editable line-numbered view must reappear, not a frozen
        // preview.
        let source_bounds = cx
            .debug_bounds("choice-markdown-view-toggle-0")
            .expect("the Source segment must have painted");
        cx.simulate_click(source_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.markdown_view),
            markdown_preview::MarkdownView::Source
        );
    }

    /// GitHub issue #145/#127-adjacent regression class: opening a *different* file must not
    /// leave a previous file's `Preview` selection stuck on screen for a file that never asked
    /// for it - `markdown_view` is one shared field, not per-tab state (see
    /// `root::AdeApp::markdown_view`'s own docs), so every file-open path must reset it exactly
    /// like `code_view` already resets.
    #[gpui::test]
    fn opening_a_different_markdown_file_resets_the_toggle_back_to_source(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("a.md"), "# a\n").expect("write a.md");
        std::fs::write(repo.path().join("b.md"), "# b\n").expect("write b.md");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("a.md"), window, cx);
        });
        cx.run_until_parked();
        let preview_bounds = cx
            .debug_bounds("choice-markdown-view-toggle-1")
            .expect("the Preview segment must have painted");
        cx.simulate_click(preview_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.markdown_view),
            markdown_preview::MarkdownView::Preview
        );

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("b.md"), window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.markdown_view),
            markdown_preview::MarkdownView::Source,
            "a newly opened file must never inherit a different file's stray Preview selection"
        );
    }

    /// Opens `contents` as a real `.md` file already switched to Preview, by really clicking the
    /// real toggle - the same path a user takes, not a direct field poke.
    fn open_markdown_preview<'a>(
        cx: &'a mut TestAppContext,
        contents: &str,
    ) -> (
        gpui::Entity<AdeApp>,
        &'a mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        std::fs::write(repo.path().join("readme.md"), contents).expect("write readme.md");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(repo.path().join("readme.md"), window, cx);
        });
        cx.run_until_parked();
        let preview_bounds = cx
            .debug_bounds("choice-markdown-view-toggle-1")
            .expect("the Preview segment must have painted");
        cx.simulate_click(preview_bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        (app, cx, repo)
    }

    /// A real click a few pixels into the first rendered paragraph - i.e. on its own first word,
    /// which every test below arranges to be the link text (or deliberately not). Deliberately not
    /// `bounds.center()`: a prose paragraph's `div` stretches the full content width, so its
    /// centre lands in empty space well past the end of a short line, where
    /// `TextLayout::index_for_position` (`vendor/zed/crates/gpui/src/elements/text.rs:829`)
    /// genuinely returns `Err` and no click range can match.
    fn click_first_paragraph_start(cx: &mut gpui::VisualTestContext) {
        let bounds = cx
            .debug_bounds("markdown-prose-0")
            .expect("the first preview paragraph must have painted");
        let point = bounds.origin + gpui::point(gpui::px(6.0), bounds.size.height / 2.0);
        cx.simulate_click(point, gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// GitHub issue #201, "Markdown links do not work" - the real, reported bug, end to end.
    ///
    /// The parse side already produced a real destination; the render side threw it away
    /// (`build_text_runs` matched `destination: _`), so a link painted as coloured text that did
    /// nothing at all when clicked. This clicks a real rendered link in a real Preview tab and
    /// asserts the app really asked the platform to open that exact URL - `cx.opened_url()` reads
    /// what `gpui::App::open_url` actually handed the platform layer
    /// (`vendor/zed/crates/gpui/src/platform/test/platform.rs:418`), so nothing here is stubbed on
    /// this app's side of the call.
    #[gpui::test]
    fn clicking_a_real_preview_link_really_opens_its_url(cx: &mut TestAppContext) {
        let (_app, cx, _repo) = open_markdown_preview(
            cx,
            "[the real documentation](https://example.com/real-target)\n",
        );

        assert_eq!(
            cx.opened_url(),
            None,
            "sanity: nothing has been opened before the click"
        );
        click_first_paragraph_start(cx);
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://example.com/real-target"),
            "clicking a real rendered Markdown link must really open its real destination"
        );
    }

    /// The other half of "clickable": clicking the ordinary prose *around* a link must not open
    /// anything. Without this, a test that only ever clicks a link cannot tell a real per-range
    /// hit test apart from a paragraph-wide "any click opens the first URL" handler - which would
    /// be exactly the kind of fake this fix must not be.
    #[gpui::test]
    fn clicking_ordinary_prose_next_to_a_link_opens_nothing(cx: &mut TestAppContext) {
        let (_app, cx, _repo) = open_markdown_preview(
            cx,
            "ordinary words first, then [a link](https://example.com/should-not-open)\n",
        );

        click_first_paragraph_start(cx);
        assert_eq!(
            cx.opened_url(),
            None,
            "a click on the plain prose at the start of the paragraph must not open the link \
             that happens to sit later in the same paragraph"
        );
    }

    /// The deliberate, documented limit (`markdown_preview::openable_url`), pinned as real
    /// behaviour rather than left to drift: a relative destination is not something this app can
    /// resolve, so clicking it must do *nothing* rather than hand a bare relative path to the OS
    /// default handler, which would resolve it against the app's working directory and open the
    /// wrong thing.
    #[gpui::test]
    fn clicking_a_relative_link_opens_nothing_rather_than_the_wrong_thing(cx: &mut TestAppContext) {
        let (_app, cx, _repo) =
            open_markdown_preview(cx, "[the contributing guide](./CONTRIBUTING.md)\n");

        click_first_paragraph_start(cx);
        assert_eq!(
            cx.opened_url(),
            None,
            "a relative destination must never reach the OS default-open handler"
        );
    }
}
