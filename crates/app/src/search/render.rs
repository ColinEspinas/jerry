//! The real GPUI Search panel - the middle tab of `Files · Search · Changes`, as `impl AdeApp`
//! methods.
//!
//! Built from top to bottom exactly as `Jerry.dc.html`'s own `showFind` block is:
//!
//! - the **query row** (30 high): leading `⌕`, the real query input, then the `Aa` / `ab` / `.*`
//!   modifier buttons,
//! - the **replace row** (28) behind `⇄`, with `Replace all` when there is something to replace,
//! - the two **glob rows** (25 each) behind the funnel: `include` and `exclude`,
//! - the **count row** (24): the count, then `⇄`, the funnel, a divider, and fold-all,
//! - the **body**: the two-level match tree, or one of the message states.
//!
//! Every one of those four fields is a real, editable `crate::text_history::TextField` with its
//! own focus handle and its own caret - `REVISION-2026-08-14.md` §5: "A fake field directly below
//! a real one is a dead end the user will click."
//!
//! The state machine behind all of it is `crate::search::state::SearchPanel`, and the searching
//! and replacing themselves are `crate::search::engine`'s. This module only draws, routes
//! keystrokes, and owns the two background tasks.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{div, font, prelude::*, px, ClickEvent, Context, KeyDownEvent, SharedString, Window};

use crate::icons::{Icon, IconRow, IconSize};
use crate::root::widgets::{text_tooltip, SimpleInput};
use crate::root::{plural, scrollbar, AdeApp, SearchInWorktree, TextRedo, TextUndo};
use crate::search::engine::{
    self, Matcher, PathFilter, ReplaceOutcome, SearchOutcome, SearchRequest,
};
use crate::search::state::{BodyState, CompletedSearch, SearchField, SearchModifier};
use crate::sidebar::file_tree::lang_chip_for_name;
use crate::sidebar::render::RightSidebarView;
use crate::theme;

/// How long the panel waits after the last keystroke before it really walks the worktree.
///
/// A search is real filesystem work over a whole checkout, so running one per keystroke would
/// start (and then have to discard) a walk for every prefix of what the user is typing. Sits at
/// the same value as `crate::code_surface::editing::REHIGHLIGHT_DEBOUNCE`, and for the same
/// reason: it must fire *within* a typing burst, not survive one.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

impl AdeApp {
    /// The whole Search tab, under the panel header the `Files · Search · Changes` toggle draws.
    pub(crate) fn render_search_panel(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let state = self.search.body_state();
        let branch = self.search_branch_label();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(self.render_search_query_block(&state, cx))
            .child(self.render_search_count_row(&state, cx))
            .children(self.render_search_notice())
            .child(self.render_search_body(&state, &branch, cx))
            .into_any_element()
    }

    /// The worktree this search is scoped to, named the way the design's two body sentences name
    /// it (`Search the files in <branch>.`).
    ///
    /// `STAGE-A-CHANGELOG.md` §4u: search is "scoped to the active worktree like the tree beside
    /// it - a hit in another checkout is not something you can act on from here without switching
    /// first". So this is the same root `crate::sidebar::render::AdeApp::render_file_tree` walks,
    /// `AdeApp::file_tree_root`, and the branch label is that worktree's own. A detached or
    /// unnamed checkout falls back to the directory name rather than printing nothing, since the
    /// sentence has to name *something* the user can recognise.
    fn search_branch_label(&self) -> String {
        let root = &self.file_tree_root;
        self.worktrees
            .iter()
            .find(|item| item.path == *root)
            .and_then(|item| item.branch.clone())
            .or_else(|| {
                root.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| root.display().to_string())
    }

    /// The query row plus whichever of the replace/include/exclude rows are revealed - one block
    /// with one bottom rule, as the mock draws it.
    fn render_search_query_block(
        &self,
        state: &BodyState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let icons = IconRow::new(&self.settings.icon_pack, IconSize::Control);
        div()
            .flex_none()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(theme::border::ROW)
            .child(
                self.search_input_row(SearchField::Query, cx)
                    .h(theme::band::FILTER_ROW)
                    .child(
                        // The leading mark. A real magnifying glass rather than the mock's
                        // hand-drawn `/`: this row is what the panel's own tab icon points at, and
                        // `REVISION-2026-08-14.md` §8 moved every glyph in this window to Phosphor.
                        icons
                            .draw(Icon::MagnifyingGlass, theme::text::DISABLED)
                            .flex_none(),
                    )
                    .child(self.render_search_field(SearchField::Query, "search this worktree"))
                    .children(
                        SearchModifier::ALL
                            .into_iter()
                            .map(|modifier| self.render_search_modifier(modifier, cx)),
                    ),
            )
            .when(self.search.replace_open, |el| {
                el.child(
                    self.search_input_row(SearchField::Replace, cx)
                        .h(theme::band::SEARCH_REPLACE_ROW)
                        .border_t_1()
                        .border_color(theme::border::ROW)
                        .child(
                            icons
                                .draw(Icon::ArrowsLeftRight, theme::text::DISABLED)
                                .flex_none(),
                        )
                        .child(
                            self.render_search_field(SearchField::Replace, "replace with\u{2026}"),
                        )
                        // `REVISION-2026-08-14.md` §7 rule 2: a control that acts on results does
                        // not exist when there are none.
                        .children(
                            state
                                .has_results()
                                .then(|| self.render_replace_all_button(cx)),
                        ),
                )
            })
            .when(self.search.globs_open, |el| {
                el.child(self.render_search_glob_row(SearchField::Include, cx))
                    .child(self.render_search_glob_row(SearchField::Exclude, cx))
            })
    }

    /// One `include` / `exclude` row: a fixed-width label, then the real field.
    fn render_search_glob_row(
        &self,
        field: SearchField,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (label, placeholder) = match field {
            SearchField::Include => ("include", "src/**, tests/**"),
            _ => ("exclude", "target/**, *.lock"),
        };
        self.search_input_row(field, cx)
            .h(theme::band::SEARCH_GLOB_ROW)
            .border_t_1()
            .border_color(theme::border::ROW)
            .child(
                div()
                    .flex_none()
                    // A fixed 44 so the two fields' left edges line up under each other - the
                    // mock's own `width:44px`. Two intrinsically-sized labels would put `include`
                    // and `exclude`'s inputs at different columns.
                    .w(px(44.0))
                    .whitespace_nowrap()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::GHOST)
                    .child(label),
            )
            .child(self.render_search_field(field, placeholder))
    }

    /// The shared shell every one of the four input rows is built on: the focus handle it tracks,
    /// the `"text-input"` context that makes Ctrl+Z mean *this* field, its key handler, and the
    /// click that focuses it.
    ///
    /// One function rather than four copies specifically because the omission this codebase keeps
    /// finding (GitHub issue #45, five times over) is a field wired into some of those four and
    /// not the rest.
    fn search_input_row(
        &self,
        field: SearchField,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!(
                "search-row-{}",
                field_key(field)
            )))
            .track_focus(self.search.focus_handle(field))
            // See `crate::default_key_bindings`' `TextUndo`/`TextRedo` docs for why the tag and
            // the listeners both live on the exact node that carries the focus.
            .key_context("text-input")
            .on_action(cx.listener(move |this, _: &TextUndo, _window, cx| {
                if this.search.field_mut(field).undo() {
                    this.on_search_input_changed(cx);
                }
            }))
            .on_action(cx.listener(move |this, _: &TextRedo, _window, cx| {
                if this.search.field_mut(field).redo() {
                    this.on_search_input_changed(cx);
                }
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                this.handle_search_key_down(field, event, window, cx);
            }))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.focus_search_field(field, window, cx);
            }))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(7.0))
            .pl(px(12.0))
            .pr(px(8.0))
    }

    /// One field's caret+text pair, through the one helper that owns that structure.
    fn render_search_field(&self, field: SearchField, placeholder: &str) -> impl IntoElement {
        let key = field_key(field);
        let (text_size, text_color) = match field {
            SearchField::Query => (11.0, theme::text::SELECTED),
            SearchField::Replace => (11.0, theme::text::STRONG),
            SearchField::Include | SearchField::Exclude => (10.0, theme::text::STRONG),
        };
        self.render_simple_input_row(SimpleInput {
            caret_selector: SharedString::from(format!("search-{key}-caret")),
            text_selector: SharedString::from(format!("search-{key}-text")),
            focus_handle: Some(self.search.focus_handle(field)),
            text: self.search.field(field).as_str(),
            caret_offset: self.search.field(field).caret(),
            placeholder,
            font: theme::font::MONO,
            text_size: self.ui_text_size(text_size),
            text_color,
            // GitHub issue #162 / §4w: the mock's browser-default placeholder "was brighter than
            // either dim-text token and absent from the palette". This is the design's own
            // `#4e545a`, through the theme layer.
            placeholder_color: theme::text::GHOST,
        })
    }

    /// One 17x17 modifier button - `Aa` / `ab` / `.*`.
    fn render_search_modifier(
        &self,
        modifier: SearchModifier,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on = modifier.is_on(self.search.options);
        let key = match modifier {
            SearchModifier::MatchCase => "case",
            SearchModifier::WholeWord => "word",
            SearchModifier::Regex => "regex",
        };
        div()
            .id(SharedString::from(format!("search-modifier-{key}")))
            .debug_selector(move || format!("search-modifier-{key}"))
            .flex_none()
            .w(theme::band::SEARCH_ICON_BUTTON)
            .h(theme::band::SEARCH_ICON_BUTTON)
            .rounded(theme::radius::CHIP)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(on, |el| el.bg(theme::search::MODIFIER_ON_BG))
            .when(!on, |el| {
                el.hover(|el| el.bg(theme::surface::SEGMENT_ACTIVE))
            })
            .tooltip(text_tooltip(modifier.tooltip()))
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(8.5))
                    .text_color(if on {
                        theme::search::MODIFIER_ON_FG
                    } else {
                        theme::text::FAINTER
                    })
                    // `ab` is underlined, the way VS Code draws whole-word
                    // (`STAGE-A-CHANGELOG.md` §4v) - the one thing that tells it apart from two
                    // ordinary letters.
                    .when(modifier == SearchModifier::WholeWord, |el| el.underline())
                    .child(modifier.label()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                modifier.toggle(&mut this.search.options);
                this.on_search_input_changed(cx);
            }))
    }

    /// `Replace all`, with the tooltip the issue calls out the mock for hardcoding.
    fn render_replace_all_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tooltip = self
            .search
            .replace_all_tooltip()
            .unwrap_or_else(|| "Replace all".to_string());
        div()
            .id("search-replace-all")
            .debug_selector(|| "search-replace-all".to_string())
            .flex_none()
            .h(px(19.0))
            .px(px(7.0))
            .rounded(theme::radius::CHIP)
            .flex()
            .items_center()
            .cursor_pointer()
            .bg(theme::surface::ROW_SELECTED)
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .tooltip(text_tooltip(tooltip))
            .child(
                div()
                    .whitespace_nowrap()
                    .font(font(theme::font::SANS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_size(self.ui_text_size(10.0))
                    .text_color(theme::text::DIM)
                    .child("Replace all"),
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                let paths = this.search.result_paths();
                this.replace_search_matches(paths, cx);
            }))
    }

    /// The count row: the count, then `⇄`, the funnel, a divider, and fold-all.
    fn render_search_count_row(
        &self,
        state: &BodyState,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let icons = IconRow::new(&self.settings.icon_pack, IconSize::Control);
        let count = self.search.count_label();
        let truncation = self.search.truncation_tooltip();
        let all_collapsed = self.search.all_collapsed();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(theme::band::SEARCH_COUNT_ROW)
            .pl(px(12.0))
            .pr(px(10.0))
            .border_b_1()
            .border_color(theme::border::ROW)
            .child(
                div()
                    .id("search-count")
                    .debug_selector(|| "search-count".to_string())
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(if matches!(state, BodyState::InvalidQuery(_)) {
                        theme::status::FAIL
                    } else {
                        theme::text::FAINTER
                    })
                    .when_some(truncation, |el, tooltip| el.tooltip(text_tooltip(tooltip)))
                    .child(count),
            )
            .child(self.render_search_toggle_button(
                &icons,
                "search-toggle-replace",
                Icon::ArrowsLeftRight,
                self.search.replace_open,
                if self.search.replace_open {
                    "Hide replace"
                } else {
                    "Show replace"
                },
                cx,
                |this, window, cx| {
                    this.search.replace_open = !this.search.replace_open;
                    // Revealing a field and leaving the caret somewhere else is half an
                    // affordance; hiding one the user is typing into would strand the focus on an
                    // unrendered node, which is exactly the dangling-focus bug class
                    // `crate::root::focus` exists for.
                    if this.search.replace_open {
                        this.focus_search_field(SearchField::Replace, window, cx);
                    } else if this.search.focused_field == SearchField::Replace {
                        this.focus_search_field(SearchField::Query, window, cx);
                    }
                    cx.notify();
                },
            ))
            .child(self.render_search_toggle_button(
                &icons,
                "search-toggle-globs",
                Icon::Funnel,
                self.search.globs_open,
                if self.search.globs_open {
                    "Hide path filters"
                } else {
                    "Limit the search to paths"
                },
                cx,
                |this, window, cx| {
                    this.search.globs_open = !this.search.globs_open;
                    if this.search.globs_open {
                        this.focus_search_field(SearchField::Include, window, cx);
                    } else if matches!(
                        this.search.focused_field,
                        SearchField::Include | SearchField::Exclude
                    ) {
                        this.focus_search_field(SearchField::Query, window, cx);
                    }
                    cx.notify();
                },
            ))
            .child(
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(11.0))
                    .bg(theme::border::DIVIDER),
            )
            // Rule 2 again: with no results there is nothing to fold, and a caret offering to
            // expand nothing is exactly what §4w records removing.
            .children(state.has_results().then(|| {
                div()
                    .id("search-fold-all")
                    .debug_selector(|| "search-fold-all".to_string())
                    .flex_none()
                    .w(theme::band::SEARCH_ICON_BUTTON)
                    .h(theme::band::SEARCH_ICON_BUTTON)
                    .rounded(theme::radius::CHIP)
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|el| el.bg(theme::surface::MENU_ROW_HOVER))
                    .tooltip(text_tooltip(if all_collapsed {
                        "Expand all files"
                    } else {
                        "Collapse all files"
                    }))
                    // The same caret a file row uses, at the same 17x17 - §4w: "semantically
                    // exact (the same action applied to every row) and from a different glyph
                    // family [than the funnel beside it], so the two cannot be confused".
                    .child(render_search_caret(!all_collapsed, self.ui_text_size(11.0)))
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.search.toggle_fold_all();
                        cx.notify();
                    }))
            }))
    }

    /// One 17x17 toggle in the count row - `⇄` or the funnel, both drawn in the active pair
    /// `REVISION-2026-08-14.md` §5 gives the modifier buttons.
    #[allow(clippy::too_many_arguments)]
    fn render_search_toggle_button(
        &self,
        icons: &IconRow<'_>,
        id: &'static str,
        icon: Icon,
        on: bool,
        tooltip: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex_none()
            .w(theme::band::SEARCH_ICON_BUTTON)
            .h(theme::band::SEARCH_ICON_BUTTON)
            .rounded(theme::radius::CHIP)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .when(on, |el| el.bg(theme::search::MODIFIER_ON_BG))
            .when(!on, |el| {
                el.hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            })
            .tooltip(text_tooltip(tooltip))
            .child(icons.draw(
                icon,
                if on {
                    theme::search::MODIFIER_ON_FG
                } else {
                    theme::text::FAINTER
                },
            ))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                on_click(this, window, cx);
            }))
    }

    /// What the last replace really did, said out loud rather than left for the user to infer
    /// from the tree redrawing - the issue's own "Replace all / per-file replace perform real
    /// edits **and report what changed**".
    fn render_search_notice(&self) -> Option<impl IntoElement> {
        let notice = self.search.notice.clone()?;
        Some(
            div()
                .debug_selector(|| "search-notice".to_string())
                .flex_none()
                .px(px(12.0))
                .py(px(6.0))
                .border_b_1()
                .border_color(theme::border::ROW)
                .font(font(theme::font::SANS))
                .text_size(self.ui_text_size(10.5))
                .text_color(theme::text::DIM)
                .child(notice),
        )
    }

    /// The body: the two-level tree, or the one sentence that state has instead.
    fn render_search_body(
        &self,
        state: &BodyState,
        branch: &str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        if let Some(message) = self.search.body_message(branch) {
            let color = match state {
                // §5's table gives the two message states two different tints, and the difference
                // is the point: "not searched yet" is dimmer than "searched, found nothing".
                BodyState::NotSearched => theme::text::HINT,
                BodyState::InvalidQuery(_) => theme::status::FAIL,
                _ => theme::text::GHOST,
            };
            return div()
                .id("search-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    div()
                        .debug_selector(|| "search-message".to_string())
                        .px(px(12.0))
                        .py(px(14.0))
                        .font(font(theme::font::SANS))
                        .text_size(self.ui_text_size(11.0))
                        .line_height(self.ui_text_size(16.0))
                        .text_color(color)
                        .child(message),
                )
                .into_any_element();
        }

        let Some(outcome) = self.search.results() else {
            return div().flex_1().min_h_0().into_any_element();
        };
        div()
            .id("search-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.search_scroll_handle)
            .children(
                outcome
                    .files
                    .iter()
                    .enumerate()
                    .map(|(index, file)| self.render_search_file(index, file, cx)),
            )
            // The app's shared overlay scrollbar, off the same handle this scroller tracks - not a
            // second, parallel tracking mechanism. Drawn as a sibling of the rows for the same
            // reason `crate::sidebar::render::AdeApp::render_file_tree` draws its own that way.
            .children(scrollbar::render_vertical_scrollbar(
                "search-scrollbar",
                &self.search_scroll_handle,
                &[],
                cx,
            ))
            .relative()
            .into_any_element()
    }

    /// One file's row plus, unless it is collapsed, one row per match under it.
    fn render_search_file(
        &self,
        index: usize,
        file: &engine::FileMatches,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open = !self.search.collapsed.contains(&file.path);
        // Built eagerly rather than inside the `.children(..)` closure below: that closure is
        // `FnMut` and would have to capture `cx` by move, which the borrow checker rightly
        // refuses. One `Vec` of already-rendered rows is also one place the "one row per match,
        // not per line" rule lives.
        let match_rows: Vec<gpui::AnyElement> = if open {
            file.lines
                .iter()
                .flat_map(|line| {
                    line.ranges
                        .iter()
                        .enumerate()
                        .map(move |(hit, range)| (line, hit, range))
                })
                .map(|(line, hit, range)| {
                    self.render_search_match(file, line, hit, range, index, cx)
                })
                .collect()
        } else {
            Vec::new()
        };
        let chip = lang_chip_for_name(file.file_name());
        let path = file.path.clone();
        let replace_path = file.path.clone();
        let count = file.match_count();
        div()
            .child(
                div()
                    .id(SharedString::from(format!("search-file-{index}")))
                    .debug_selector(move || format!("search-file-{index}"))
                    .group(SharedString::from(format!("search-file-group-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .h(theme::band::SEARCH_FILE_ROW)
                    .pl(px(6.0))
                    .pr(px(10.0))
                    .cursor_pointer()
                    .bg(theme::surface::LSP_POPOVER_FOOTER)
                    .border_t_1()
                    .border_color(theme::border::RAIL_INNER)
                    .hover(|el| el.bg(theme::surface::ROW_SELECTED))
                    .tooltip(text_tooltip(file.relative.clone()))
                    .child(
                        div()
                            .flex_none()
                            .w(px(11.0))
                            .text_center()
                            .child(render_search_caret(open, self.ui_text_size(8.0))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(14.0))
                            .h(px(14.0))
                            .rounded(theme::radius::CHIP)
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(chip.bg)
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(7.0))
                            .text_color(chip.fg)
                            .child(chip.label),
                    )
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .font(font(theme::font::MONO))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_size(self.ui_text_size(10.5))
                            .text_color(theme::text::STRONG)
                            .child(file.file_name().to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::GHOSTER)
                            .child(file.directory().to_string()),
                    )
                    // Per-file replace, revealed on hover and only while the replace row is open -
                    // acting on a field the user cannot see would be a control with no visible
                    // subject. The mock has no such button; the issue's acceptance criterion names
                    // per-file replace explicitly, so it is here rather than nowhere.
                    .when(self.search.replace_open, |el| {
                        let group = SharedString::from(format!("search-file-group-{index}"));
                        el.child(
                            div()
                                .id(SharedString::from(format!("search-file-replace-{index}")))
                                .debug_selector(move || format!("search-file-replace-{index}"))
                                .flex_none()
                                .w(px(15.0))
                                .h(px(15.0))
                                .rounded(theme::radius::CHIP)
                                .flex()
                                .items_center()
                                .justify_center()
                                .invisible()
                                .group_hover(group, |el| el.visible())
                                .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                                .tooltip(text_tooltip(format!(
                                    "Replace {} in {}",
                                    plural::count(count, "match", Some("matches")),
                                    file.file_name()
                                )))
                                .child(
                                    div()
                                        .font(font(theme::font::MONO))
                                        .text_size(self.ui_text_size(10.0))
                                        .text_color(theme::text::FAINTER)
                                        .child("\u{21c4}"),
                                )
                                .on_click(cx.listener(
                                    move |this, event: &ClickEvent, _window, cx| {
                                        // Stops the click from also toggling the row's own collapse -
                                        // a replace that folds the file it just changed hides the
                                        // result it is meant to report.
                                        cx.stop_propagation();
                                        let _ = event;
                                        this.replace_search_matches(vec![replace_path.clone()], cx);
                                    },
                                )),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .font(font(theme::font::MONO))
                            .text_size(self.ui_text_size(9.5))
                            .text_color(theme::text::FAINTER)
                            .child(count.to_string()),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        if !this.search.collapsed.remove(&path) {
                            this.search.collapsed.insert(path.clone());
                        }
                        cx.notify();
                    })),
            )
            .when(open, |el| {
                el.child(
                    div()
                        .relative()
                        .pt(px(2.0))
                        .pb(px(3.0))
                        // The vertical rule under the file row's caret, tying its match rows to
                        // it - the same indent guide the file tree draws.
                        .child(
                            div()
                                .absolute()
                                .left(px(11.0))
                                .top_0()
                                .bottom_0()
                                .w(px(1.0))
                                .bg(theme::border::DIVIDER),
                        )
                        .children(match_rows),
                )
            })
            .into_any_element()
    }

    /// One match row: the line number, then the line with **this** hit highlighted.
    ///
    /// One row per *match*, not per line (`STAGE-A-CHANGELOG.md` §4v: "one row per match with its
    /// line number and the hit highlighted"), so two hits on one line are two rows that each
    /// point at their own - which is also what makes the count row's total and the rows on screen
    /// agree.
    #[allow(clippy::too_many_arguments)]
    fn render_search_match(
        &self,
        file: &engine::FileMatches,
        line: &engine::LineMatch,
        hit: usize,
        range: &std::ops::Range<usize>,
        file_index: usize,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (before, matched, after) = engine::elide_around(&line.text, range);
        let path = file.path.clone();
        let line_number = line.line_number;
        let id = SharedString::from(format!("search-match-{file_index}-{line_number}-{hit}"));
        div()
            .id(id.clone())
            .debug_selector(move || id.to_string())
            .flex()
            .items_baseline()
            .gap(px(8.0))
            .h(theme::band::SEARCH_MATCH_ROW)
            .pl(px(18.0))
            .pr(px(10.0))
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER))
            .tooltip(text_tooltip(format!(
                "{}:{line_number} \u{2014} open",
                file.relative
            )))
            .child(
                div()
                    .flex_none()
                    .w(px(26.0))
                    .text_right()
                    .whitespace_nowrap()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(9.5))
                    .text_color(theme::text::DISABLED)
                    .child(line_number.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(10.0))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::search::LINE)
                            .child(before),
                    )
                    .child(
                        div()
                            .flex_none()
                            .bg(theme::search::MATCH_BG)
                            .text_color(theme::search::MATCH_FG)
                            .child(matched),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::search::LINE)
                            .child(after),
                    ),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_search_match(path.clone(), line_number, window, cx);
            }))
            .into_any_element()
    }
}

/// `▾` open / `▸` closed - the file tree's own caret glyphs, reused verbatim so the two trees in
/// this window speak one language (`STAGE-A-CHANGELOG.md` §4w's fold-all note is explicit that
/// this is "the same caret a file row uses").
fn render_search_caret(open: bool, text_size: gpui::Pixels) -> impl IntoElement {
    div()
        .font(font(theme::font::MONO))
        .text_size(text_size)
        .text_color(theme::text::TREE_CARET)
        .child(if open { "\u{25be}" } else { "\u{25b8}" })
}

/// A field's stable slug, for element ids and debug selectors.
fn field_key(field: SearchField) -> &'static str {
    match field {
        SearchField::Query => "query",
        SearchField::Replace => "replace",
        SearchField::Include => "include",
        SearchField::Exclude => "exclude",
    }
}

impl AdeApp {
    /// `mod+shift+F` - the issue's own binding, "opens the panel focused in the query".
    pub(crate) fn handle_search_in_worktree_action(
        &mut self,
        _: &SearchInWorktree,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search_panel(window, cx);
    }

    /// Opens the Search tab with the query focused - `mod+shift+F`, and the panel tab's own click
    /// when it is already showing.
    pub(crate) fn open_search_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.right_sidebar_view != RightSidebarView::Search {
            self.set_right_sidebar_view(RightSidebarView::Search, window, cx);
        }
        self.focus_search_field(SearchField::Query, window, cx);
        cx.notify();
    }

    /// Moves focus - and the key handler's own idea of which field is being typed into - to
    /// `field`.
    pub(crate) fn focus_search_field(
        &mut self,
        field: SearchField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search.focused_field = field;
        let handle = self.search.focus_handle(field).clone();
        window.focus(&handle, cx);
        self.reset_caret_blink(cx);
        cx.notify();
    }

    /// One keystroke into one of the four fields.
    ///
    /// Everything but `Tab` and `Esc` is `TextField::handle_editing_key`'s, so all four fields get
    /// the same real editing vocabulary - caret movement included - rather than each row growing
    /// its own half of it.
    fn handle_search_key_down(
        &mut self,
        field: SearchField,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        if keystroke.modifiers.platform || keystroke.modifiers.control || keystroke.modifiers.alt {
            return;
        }
        self.reset_caret_blink(cx);
        match keystroke.key.as_str() {
            // Walks only the rows that are really on screen - see
            // `SearchPanel::next_visible_field`.
            "tab" => {
                if let Some(next) = self.search.next_visible_field(field) {
                    self.focus_search_field(next, window, cx);
                }
                cx.stop_propagation();
                return;
            }
            // A real, undoable step rather than a silent loss, exactly as the rail filter's own
            // `Esc` is.
            "escape" => {
                if self.search.field_mut(field).clear(Instant::now()) {
                    self.on_search_input_changed(cx);
                    cx.stop_propagation();
                }
                return;
            }
            _ => {}
        }
        if self.search.field_mut(field).handle_editing_key(
            keystroke.key.as_str(),
            keystroke.key_char.as_deref(),
            Instant::now(),
        ) {
            self.on_search_input_changed(cx);
            cx.stop_propagation();
        }
    }

    /// Anything that changes what the search would return: a keystroke in any of the four fields,
    /// a modifier toggle, an undo.
    ///
    /// Clears the replace notice too - it described a replace against results that no longer
    /// answer what is typed, and a stale report is worse than none.
    fn on_search_input_changed(&mut self, cx: &mut Context<Self>) {
        self.search.notice = None;
        self.start_search(cx);
        cx.notify();
    }

    /// Compiles the query and, if it compiles to something, starts a real debounced search of the
    /// active worktree on the background executor.
    ///
    /// Generation-guarded rather than task-cancelled: a slow search over a big worktree must never
    /// overwrite a newer, faster one's results, and the check is one comparison at the moment the
    /// result lands. The debounce is `SEARCH_DEBOUNCE` - see its own docs for why a search is not
    /// run per keystroke.
    pub(crate) fn start_search(&mut self, cx: &mut Context<Self>) {
        self.search.generation += 1;
        let generation = self.search.generation;

        let matcher = match Matcher::compile(self.search.query.as_str(), self.search.options) {
            Ok(Some(matcher)) => {
                self.search.error = None;
                matcher
            }
            // An empty query is the not-searched-yet state, not a search of nothing.
            Ok(None) => {
                self.search.error = None;
                self.search.searching = false;
                self.search.completed = None;
                self.search.collapsed.clear();
                return;
            }
            Err(error) => {
                self.search.error = Some(error.0);
                self.search.searching = false;
                return;
            }
        };

        let request = SearchRequest {
            root: self.file_tree_root.clone(),
            matcher,
            filter: PathFilter::new(self.search.include.as_str(), self.search.exclude.as_str()),
        };
        // Captured now rather than re-read after the awaits below: a search that resolves against
        // whatever the user has since typed would report a count for one query beside a tree for
        // another. Same rule `crate::code_surface::editing::AdeApp::enqueue_save` follows for its
        // own worktree root.
        let query = self.search.query.as_str().to_string();
        let options = self.search.options;
        let include = self.search.include.as_str().to_string();
        let exclude = self.search.exclude.as_str().to_string();

        self.search.searching = true;
        self._search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            let still_current = this
                .update(cx, |this, _cx| this.search.generation == generation)
                .unwrap_or(false);
            if !still_current {
                return;
            }
            let outcome: SearchOutcome = cx
                .background_executor()
                .spawn(async move { engine::search_worktree(&request) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.search.generation != generation {
                    return;
                }
                this.search.searching = false;
                // A fresh result set opens every file, which is the state the design draws - the
                // previous search's collapse decisions were about different files.
                this.search.collapsed.clear();
                this.search.completed = Some(CompletedSearch {
                    query,
                    options,
                    include,
                    exclude,
                    outcome,
                });
                cx.notify();
            });
        }));
    }

    /// Replaces every match in `paths` for real, on disk, then re-runs the search so the tree
    /// reflects what the files now hold rather than what they held a moment ago.
    ///
    /// Files open in the editor with **unsaved** edits are refused and named - see
    /// `crate::search::engine::replace_across`'s own docs for why writing them would destroy
    /// those edits. The notice this leaves behind is the issue's "report what changed".
    pub(crate) fn replace_search_matches(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        let Ok(Some(matcher)) = Matcher::compile(self.search.query.as_str(), self.search.options)
        else {
            return;
        };
        let replacement = self.search.replace.as_str().to_string();
        let dirty = self.dirty_edit_buffer_paths();

        self._search_replace_task = Some(cx.spawn(async move |this, cx| {
            let outcome: ReplaceOutcome = cx
                .background_executor()
                .spawn(
                    async move { engine::replace_across(&paths, &matcher, &replacement, &dirty) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.search.notice = Some(replace_notice(&outcome));
                // A replace is a real change to the working tree, and nothing else re-derives the
                // diff from it - the same reason `spawn_file_save_loop` reloads it after a save.
                if outcome.files_changed > 0 {
                    this.load_diff(this.diff_root.clone(), cx);
                    // Force the File view's next freshness check to re-read rather than trusting
                    // its throttle window: we just changed these files' real mtimes ourselves.
                    this.file_view_last_freshness_check = None;
                }
                this.start_search(cx);
                cx.notify();
            });
        }));
    }

    /// Every open editor buffer with unsaved changes, as absolute paths - what a replace refuses
    /// to write over.
    fn dirty_edit_buffer_paths(&self) -> HashSet<PathBuf> {
        self.edit_buffers
            .values()
            .filter(|buffer| buffer.is_dirty())
            .map(|buffer| buffer.path.clone())
            .collect()
    }

    /// Opens the file a match row points at, in the editor, scrolled to that line.
    fn open_search_match(
        &mut self,
        path: PathBuf,
        line_number: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 1-based on screen, 0-based in the editor - converted here, once, rather than at either
        // end where it would be one more place to get wrong.
        self.open_file_at_line(path, line_number.saturating_sub(1), window, cx);
    }
}

/// One sentence saying exactly what a replace did, including what it refused to do.
///
/// Pure so the wording is a real unit test rather than something only a screenshot can check -
/// and there is real wording to get wrong here, since "nothing changed" and "3 files were skipped"
/// are different facts that a single count would flatten into one.
pub fn replace_notice(outcome: &ReplaceOutcome) -> String {
    let mut parts = Vec::new();
    if outcome.matches_replaced == 0 {
        parts.push("Nothing replaced".to_string());
    } else {
        parts.push(format!(
            "Replaced {} in {}",
            plural::count(outcome.matches_replaced, "match", Some("matches")),
            plural::count(outcome.files_changed, "file", None)
        ));
    }
    if !outcome.skipped_dirty.is_empty() {
        parts.push(format!(
            "{} skipped \u{2014} unsaved changes are open in the editor",
            plural::count(outcome.skipped_dirty.len(), "file", None)
        ));
    }
    if !outcome.failed.is_empty() {
        parts.push(format!(
            "{} failed to write",
            plural::count(outcome.failed.len(), "file", None)
        ));
    }
    format!("{}.", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(replaced: usize, files: usize) -> ReplaceOutcome {
        ReplaceOutcome {
            files_changed: files,
            matches_replaced: replaced,
            skipped_dirty: Vec::new(),
            failed: Vec::new(),
        }
    }

    #[test]
    fn a_real_replace_reports_what_it_changed_through_the_pluralisation_helper() {
        assert_eq!(
            replace_notice(&outcome(14, 6)),
            "Replaced 14 matches in 6 files."
        );
        assert_eq!(
            replace_notice(&outcome(1, 1)),
            "Replaced 1 match in 1 file."
        );
    }

    #[test]
    fn a_replace_that_changed_nothing_says_so_rather_than_reporting_zero_of_something() {
        assert_eq!(replace_notice(&outcome(0, 0)), "Nothing replaced.");
    }

    #[test]
    fn refused_files_are_named_as_a_separate_fact_not_folded_into_the_count() {
        let mut outcome = outcome(4, 2);
        outcome.skipped_dirty = vec![PathBuf::from("/wt/a.rs"), PathBuf::from("/wt/b.rs")];
        assert_eq!(
            replace_notice(&outcome),
            "Replaced 4 matches in 2 files; 2 files skipped \u{2014} unsaved changes are open in \
             the editor."
        );
    }

    #[test]
    fn a_write_that_really_failed_is_reported_alongside_what_succeeded() {
        let mut outcome = outcome(4, 2);
        outcome.failed = vec![(PathBuf::from("/wt/c.rs"), "permission denied".to_string())];
        assert_eq!(
            replace_notice(&outcome),
            "Replaced 4 matches in 2 files; 1 file failed to write."
        );
    }
}
