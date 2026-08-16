//! The real GPUI Search panel - the middle tab of `Files · Search · Changes`, as `impl AdeApp`
//! methods.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use gpui::{div, font, prelude::*, px, ClickEvent, Context, KeyDownEvent, SharedString, Window};

use crate::icons::{Icon, IconRow, IconSize};
use crate::root::widgets::{self, text_tooltip, SimpleInput, TextFieldHandle};
use crate::root::{plural, scrollbar, AdeApp, FindInFile, SearchInWorktree, TextRedo, TextUndo};
use crate::search::engine::{
    self, Matcher, PathFilter, ReplaceOutcome, SearchOutcome, SearchRequest,
};
use crate::search::state::{
    self, BodyState, CompletedSearch, SearchField, SearchListItem, SearchModifier,
};
use crate::sidebar::file_tree::lang_chip_for_name;
use crate::sidebar::render::RightSidebarView;
use crate::theme;

/// How long the panel waits after the last keystroke before it really walks the worktree.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(150);

/// How far past `AdeApp::search_list_state`'s own viewport `AdeApp::render_search_body`'s
/// `gpui::list` measures rows ahead of time - the same real overdraw margin
/// `crate::rail::render::RAIL_LIST_OVERDRAW`/`crate::sidebar::render::CHANGES_LIST_OVERDRAW` use
/// for the identical "a little slack so a small scroll doesn't have to measure a brand new row
/// synchronously" reason.
pub(crate) const SEARCH_LIST_OVERDRAW: gpui::Pixels = px(48.0);

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
        div()
            .flex_none()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(theme::border::ROW)
            .child(
                self.search_input_row(SearchField::Query, cx)
                    .h(theme::band::FILTER_ROW)
                    .child(render_search_row_mark("/", self.ui_text_size(10.0)))
                    .child(self.render_search_field(SearchField::Query, "search this worktree", cx))
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
                        .child(render_search_row_mark("\u{21c4}", self.ui_text_size(10.0)))
                        .child(self.render_search_field(
                            SearchField::Replace,
                            "replace with\u{2026}",
                            cx,
                        ))
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
            .child(self.render_search_field(field, placeholder, cx))
    }

    /// The shared shell every one of the four input rows is built on: the focus handle it tracks,
    /// the `"text-input"` context that makes Ctrl+Z mean *this* field, its key handler, and the
    /// click that focuses it.
    fn search_input_row(
        &self,
        field: SearchField,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let row = div()
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
            }));
        // GitHub issue #336's four clipboard/select-all actions, on the same node and for the same
        // structural-routing reason the two undo actions above are - and carrying the same
        // `on_search_input_changed` follow-up work every other edit to these fields does, so a
        // pasted or cut query really re-runs the search instead of leaving stale results up.
        self.wire_text_input_actions(row, search_field_handle(field), cx)
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
    fn render_search_field(
        &self,
        field: SearchField,
        placeholder: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let key = field_key(field);
        let (text_size, text_color) = match field {
            SearchField::Query => (11.0, theme::text::SELECTED),
            SearchField::Replace => (11.0, theme::text::STRONG),
            SearchField::Include | SearchField::Exclude => (10.0, theme::text::STRONG),
        };
        self.render_simple_input_row(
            SimpleInput {
                caret_selector: SharedString::from(format!("search-{key}-caret")),
                text_selector: SharedString::from(format!("search-{key}-text")),
                focus_handle: Some(self.search.focus_handle(field)),
                text: self.search.field(field).as_str(),
                caret_offset: self.search.field(field).caret(),
                selection: self.search.field(field).selection(),
                placeholder,
                font: theme::font::MONO,
                text_size: self.ui_text_size(text_size),
                text_color,
                // GitHub issue #162 / §4w: the mock's browser-default placeholder "was brighter than
                // either dim-text token and absent from the palette". This is the design's own
                // `#4e545a`, through the theme layer.
                placeholder_color: theme::text::GHOST,
                caret: widgets::SimpleInputCaret::default(),
                field: Some(search_field_handle(field)),
            },
            cx,
        )
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
            .w(theme::band::ICON_BUTTON_HIT)
            .h(theme::band::ICON_BUTTON_HIT)
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
            // The 1px rule exists to separate the two field toggles from fold-all. With fold-all
            // gone there is nothing on its far side, and a divider dividing something from nothing
            // is a stray mark - so it is gated on the same flag the control it separates is.
            .children(state.has_results().then(|| {
                div()
                    .flex_none()
                    .w(px(1.0))
                    .h(px(11.0))
                    .bg(theme::border::DIVIDER)
            }))
            // Rule 2 again: with no results there is nothing to fold, and a caret offering to
            // expand nothing is exactly what §4w records removing.
            .children(state.has_results().then(|| {
                div()
                    .id("search-fold-all")
                    .debug_selector(|| "search-fold-all".to_string())
                    .flex_none()
                    .w(theme::band::ICON_BUTTON_HIT)
                    .h(theme::band::ICON_BUTTON_HIT)
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

    /// One toggle in the count row - `⇄` or the funnel, both drawn in the active pair
    /// `REVISION-2026-08-14.md` §5 gives the modifier buttons. The button itself is the shared
    /// 17x17 hit box (`theme::band::ICON_BUTTON_HIT`); the glyph `icons` draws inside it is the
    /// smaller `icons::IconSize::Control` optical box (12px), centred by this row's own
    /// `items_center`/`justify_center` - see `IconSize::Control`'s doc comment for why those two
    /// numbers are not the same.
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
            .w(theme::band::ICON_BUTTON_HIT)
            .h(theme::band::ICON_BUTTON_HIT)
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
        // A real, per-render snapshot rather than a live borrow through `self.search`: `gpui::
        // list` may not actually build a given row's element until several frames after this
        // pass ran, and the row data a stale index resolves against then has to be a real,
        // captured value - mirrors `crate::rail::render::AdeApp::render_rail_list`'s own
        // `Rc<Vec<RepoGroup>>` snapshot, taken for the identical reason.
        let outcome: Rc<SearchOutcome> = Rc::new(outcome.clone());
        let items: Rc<Vec<SearchListItem>> = Rc::new(state::flatten_search_list_items(
            &outcome,
            &self.search.collapsed,
        ));
        // `ListState` owns a measured height per item, so it has to be told when the item set
        // changes size. Reset only on a real change: a reset drops the scroll position, and
        // `AdeApp::start_search` already clears `self.search.collapsed` on every fresh result set
        // (opening every file), so this is never reached with the same length meaning a genuinely
        // different tree - the same real-change gate `changes_sections_list`/`rail_list_state`
        // both use.
        if self.search_list_state.item_count() != items.len() {
            self.search_list_state.reset(items.len());
        }

        let build_items = items.clone();
        let build_outcome = outcome.clone();
        let list = gpui::list(
            self.search_list_state.clone(),
            cx.processor(
                move |this: &mut Self,
                      index: usize,
                      window: &mut Window,
                      cx: &mut Context<Self>| {
                    // Bounds-checked rather than indexed, mirroring `Self::render_rail_list`'s own
                    // dispatch: this frame's flattened snapshot may be stale by the time `gpui::
                    // list` actually asks for one of its rows, and a stale index must render
                    // nothing rather than panic.
                    match build_items.get(index) {
                        Some(item) => {
                            this.render_search_list_item(&build_outcome, item, window, cx)
                        }
                        None => div().into_any_element(),
                    }
                },
            ),
        )
        .w_full()
        .flex_1()
        .min_h_0();

        // See `Self::render_file_tree`'s own docs (mirrored by every other virtualized list in
        // this app) on why the scrollbar must be a sibling of the list, inside its own
        // non-scrolling `.relative()` wrapper: the list now owns its own scroll offset via
        // `self.search_list_state` rather than the wrapper scrolling a plain `.children(...)`.
        div()
            .id("search-body")
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            // The app's shared overlay scrollbar, off the same `ListState` the list itself scrolls
            // - not a second, parallel tracking mechanism.
            .children(scrollbar::render_vertical_scrollbar(
                "search-scrollbar",
                &self.search_list_state,
                &[],
                cx,
            ))
            .into_any_element()
    }

    /// Dispatches one flattened [`SearchListItem`] to the renderer for its kind - see that type's
    /// own docs. `outcome` is this frame's own captured snapshot (never a possibly-stale live
    /// borrow), the same defensive re-resolve `crate::rail::render::AdeApp::render_rail_list_item`
    /// already documents for the identical reason.
    fn render_search_list_item(
        &self,
        outcome: &SearchOutcome,
        item: &SearchListItem,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match *item {
            SearchListItem::FileRow { file_index } => match outcome.files.get(file_index) {
                Some(file) => {
                    let open = !self.search.collapsed.contains(&file.path);
                    self.render_search_file_row(file_index, file, open, cx)
                }
                None => div().into_any_element(),
            },
            SearchListItem::MatchRow {
                file_index,
                line_index,
                hit_index,
            } => {
                let resolved = outcome.files.get(file_index).and_then(|file| {
                    let line = file.lines.get(line_index)?;
                    let range = line.ranges.get(hit_index)?;
                    let is_first = line_index == 0 && hit_index == 0;
                    let is_last =
                        line_index + 1 == file.lines.len() && hit_index + 1 == line.ranges.len();
                    Some((file, line, range, is_first, is_last))
                });
                match resolved {
                    Some((file, line, range, is_first, is_last)) => self.render_search_match(
                        file, line, hit_index, range, file_index, is_first, is_last, cx,
                    ),
                    None => div().into_any_element(),
                }
            }
        }
    }

    /// One file's own header row - its name, its chip, its match count, its collapse caret. The
    /// match rows under it (unless it is collapsed) are separate, sibling [`SearchListItem::
    /// MatchRow`] items in the same flattened list, not children of this one - see
    /// [`SearchListItem`]'s own docs for why.
    fn render_search_file_row(
        &self,
        index: usize,
        file: &engine::FileMatches,
        open: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
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
                    // Per-file replace, shown only while the replace row is open - acting on a
                    // field the user cannot see would be a control with no visible subject. The
                    // mock has no such button; the issue's acceptance criterion names per-file
                    // replace explicitly, so it is here rather than nowhere.
                    //
                    // Always present rather than hover-only, following the file tree's own
                    // per-directory `+`: "this project has no established 'hidden until row hover'
                    // mechanism yet, and a subtle-but-always-there affordance beats an invented
                    // one" (`crate::sidebar::render::AdeApp::render_file_tree_row`). It is also
                    // the difference between a control a test can click and one it cannot.
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
                                .group_hover(group, |el| el.bg(theme::surface::ROW_HOVER_ALT))
                                .hover(|el| el.bg(theme::surface::SEGMENT_ACTIVE))
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
            .into_any_element()
    }

    /// One match row: the line number, then the line with **this** hit highlighted.
    #[allow(clippy::too_many_arguments)]
    fn render_search_match(
        &self,
        file: &engine::FileMatches,
        line: &engine::LineMatch,
        hit: usize,
        range: &std::ops::Range<usize>,
        file_index: usize,
        is_first: bool,
        is_last: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (before, matched, after) = engine::elide_around(&line.text, range);
        let path = file.path.clone();
        let line_number = line.line_number;
        let id = SharedString::from(format!("search-match-{file_index}-{line_number}-{hit}"));
        div()
            .relative()
            .when(is_first, |el| el.pt(px(2.0)))
            .when(is_last, |el| el.pb(px(3.0)))
            // The vertical rule under the file row's caret, tying this match row to it - the same
            // indent guide the file tree draws, now painted per row rather than once across the
            // old wrapper (see this function's own docs).
            .child(
                div()
                    .absolute()
                    .left(px(11.0))
                    .top_0()
                    .bottom_0()
                    .w(px(1.0))
                    .bg(theme::border::DIVIDER),
            )
            .child(
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
                    })),
            )
            .into_any_element()
    }
}

/// A field row's leading mark - the `/` of the query row and the `⇄` of the replace row.
fn render_search_row_mark(mark: &'static str, text_size: gpui::Pixels) -> impl IntoElement {
    div()
        .flex_none()
        .font(font(theme::font::MONO))
        .text_size(text_size)
        .text_color(theme::text::DISABLED)
        .child(mark)
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
/// One search field's own [`TextFieldHandle`] - what click/drag selection and GitHub issue #336's
/// four clipboard/select-all actions act on.
fn find_bar_query_handle() -> TextFieldHandle {
    TextFieldHandle::new(|app: &mut AdeApp| app.find_bar.as_mut().map(|bar| &mut bar.query))
        .on_changed(|app: &mut AdeApp, cx| {
            app.refresh_find_bar();
            app.reveal_current_find_hit(cx);
        })
}

fn search_field_handle(field: SearchField) -> TextFieldHandle {
    TextFieldHandle::new(move |app: &mut AdeApp| Some(app.search.field_mut(field)))
        .on_changed(|app: &mut AdeApp, cx| app.on_search_input_changed(cx))
}

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
    fn handle_search_key_down(
        &mut self,
        field: SearchField,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) = widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
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
            modifiers,
            Instant::now(),
        ) {
            self.on_search_input_changed(cx);
            cx.stop_propagation();
        }
    }

    /// Anything that changes what the search would return: a keystroke in any of the four fields,
    /// a modifier toggle, an undo.
    fn on_search_input_changed(&mut self, cx: &mut Context<Self>) {
        self.search.notice = None;
        self.start_search(cx);
        cx.notify();
    }

    /// Compiles the query and, if it compiles to something, starts a real debounced search of the
    /// active worktree on the background executor.
    pub(crate) fn start_search(&mut self, cx: &mut Context<Self>) {
        self.search.generation += 1;
        let generation = self.search.generation;
        self.search_generation.store(generation, Ordering::SeqCst);

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
            // GitHub issue #401: the real, persisted, user-editable list - see
            // `crate::settings::store::EditorSettings::search_excludes`'s own docs.
            search_excludes: self.settings.editor.search_excludes.clone(),
            respect_gitignore: self.settings.editor.respect_gitignore,
        };
        // Captured now rather than re-read after the awaits below: a search that resolves against
        // whatever the user has since typed would report a count for one query beside a tree for
        // another. Same rule `crate::code_surface::editing::AdeApp::enqueue_save` follows for its
        // own worktree root.
        let query = self.search.query.as_str().to_string();
        let options = self.search.options;
        let include = self.search.include.as_str().to_string();
        let exclude = self.search.exclude.as_str().to_string();

        // Cloned before the `move` below: the closure the background executor calls needs its own
        // handle to poll, independent of whatever `self.search_generation` ends up being cloned
        // into by a later keystroke's own `start_search`.
        let search_generation = self.search_generation.clone();

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
                .spawn(async move {
                    engine::search_worktree_cancellable(&request, &|| {
                        search_generation.load(Ordering::SeqCst) != generation
                    })
                })
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
        self.open_file_at_line(path, line_number, window, cx);
    }
}

/// One sentence saying exactly what a replace did, including what it refused to do.
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

/// The panel driven the way a user drives it: a real window, real keystrokes into the real
/// fields, real clicks on the real controls, and - for replace - real files on disk that really
/// change.
#[cfg(test)]
mod panel_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use crate::search::state::SearchField;
    use gpui::TestAppContext;
    use tempfile::TempDir;

    /// A real worktree on disk with `refresh_token` across four files - the same corpus
    /// `Jerry.dc.html`'s own search fixture uses, so what these tests see is what the design was
    /// drawn against.
    fn fixture_repo() -> TempDir {
        let repo = TempDir::new().expect("tempdir");
        let write = |relative: &str, content: &str| {
            let path = repo.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
            std::fs::write(&path, content).expect("write");
        };
        write(
            "src/auth/session.rs",
            "let refresh_token = store.issue(&sid)?;\nif self.refresh_token.is_expired(now) {}\n",
        );
        write(
            "src/auth/store.rs",
            "fn refresh_token(&self, sid: &SessionId) -> Option<Token>;\n",
        );
        write("src/api/users.rs", "let t = auth.refresh_token(&sid)?;\n");
        write("tests/auth_race.rs", "let a = svc.refresh_token(sid);\n");
        write("README.md", "nothing to find here\n");
        repo
    }

    fn open_search<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| app.open_search_panel(window, cx));
        cx.run_until_parked();
        (app, cx)
    }

    /// Types into whichever field is focused and lets the debounced search really finish.
    fn type_and_settle(cx: &mut gpui::VisualTestContext, text: &str) {
        cx.simulate_input(text);
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
        cx.run_until_parked();
    }

    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("`{selector}` must really paint"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_count_row_toggle_icons_paint_smaller_than_their_hit_box(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (_app, cx) = open_search(cx, &repo);

        for (button_selector, icon_selector) in [
            ("search-toggle-replace", "icon-arrows-left-right"),
            ("search-toggle-globs", "icon-funnel"),
        ] {
            let button = cx
                .debug_bounds(button_selector)
                .unwrap_or_else(|| panic!("`{button_selector}` must really paint"));
            assert_eq!(button.size.width, px(17.0), "{button_selector}'s hit box");
            assert_eq!(button.size.height, px(17.0), "{button_selector}'s hit box");

            let icon = cx
                .debug_bounds(icon_selector)
                .unwrap_or_else(|| panic!("`{icon_selector}` must paint inside {button_selector}"));
            assert_eq!(
                icon.size.width,
                px(12.0),
                "{icon_selector} must paint at IconSize::Control's real optical box (12px), not \
                 stretched to fill the 17px hit box"
            );
            assert_eq!(icon.size.height, px(12.0));

            let left_gap = icon.origin.x - button.origin.x;
            let right_gap =
                (button.origin.x + button.size.width) - (icon.origin.x + icon.size.width);
            let top_gap = icon.origin.y - button.origin.y;
            let bottom_gap =
                (button.origin.y + button.size.height) - (icon.origin.y + icon.size.height);
            assert_eq!(
                left_gap, right_gap,
                "{icon_selector} must be horizontally centred in {button_selector}"
            );
            assert_eq!(
                top_gap, bottom_gap,
                "{icon_selector} must be vertically centred in {button_selector}"
            );
        }
    }

    #[gpui::test]
    fn mod_shift_f_opens_the_panel_with_the_query_focused(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.dispatch_action(SearchInWorktree);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(app.right_sidebar_view, RightSidebarView::Search);
        });
        let (focused, query_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.search.query_focus_handle.clone())
        });
        assert_eq!(
            focused.as_ref(),
            Some(&query_handle),
            "the issue's own wording: \"opens the panel focused in the query\""
        );
    }

    #[gpui::test]
    fn typing_a_real_query_really_searches_the_worktree_and_builds_the_tree(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);

        app.read_with(cx, |app, _| {
            assert_eq!(app.search.body_state(), BodyState::NotSearched);
            assert_eq!(app.search.count_label(), "");
        });
        assert!(
            cx.debug_bounds("search-fold-all").is_none(),
            "a control that acts on results does not exist when there are none"
        );

        type_and_settle(cx, "refresh_token");

        app.read_with(cx, |app, _| {
            assert_eq!(app.search.query.as_str(), "refresh_token");
            assert_eq!(app.search.body_state(), BodyState::Results);
            assert_eq!(
                app.search.count_label(),
                "5 results in 4 files",
                "a real walk of a real worktree, not a fixture handed to the panel"
            );
        });
        assert!(
            cx.debug_bounds("search-file-0").is_some(),
            "the tree's first file row must really paint"
        );
        assert!(
            cx.debug_bounds("search-fold-all").is_some(),
            "and now that there are results, so must fold-all"
        );
    }

    #[gpui::test]
    fn clearing_the_query_returns_to_the_not_searched_state(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.body_state(), BodyState::Results)
        });

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.search.body_state(),
                BodyState::NotSearched,
                "the acceptance criterion: \"clearing the query returns to the not-searched state\""
            );
            assert_eq!(app.search.count_label(), "");
        });
        assert!(cx.debug_bounds("search-fold-all").is_none());
    }

    #[gpui::test]
    fn the_match_case_button_really_changes_the_result_set(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        std::fs::write(repo.path().join("src/Cased.rs"), "Refresh_Token\n").expect("write");
        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "Refresh_Token");

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.search.count_label(),
                "6 results in 5 files",
                "case-insensitive by default"
            );
        });

        click(cx, "search-modifier-case");
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(app.search.options.match_case);
            assert_eq!(
                app.search.count_label(),
                "1 result in 1 file",
                "`Aa` changes results - the acceptance criterion says so in as many words"
            );
        });
    }

    #[gpui::test]
    fn all_four_fields_are_really_editable_and_the_globs_really_narrow_the_search(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.count_label(), "5 results in 4 files")
        });

        // The funnel reveals the two glob rows and focuses the first, so the very next keystroke
        // lands in a field the user can see.
        click(cx, "search-toggle-globs");
        app.read_with(cx, |app, _| {
            assert!(app.search.globs_open);
            assert_eq!(app.search.focused_field, SearchField::Include);
        });
        type_and_settle(cx, "src/**");
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.include.as_str(), "src/**");
            assert_eq!(
                app.search.count_label(),
                "4 results in 3 files",
                "the include glob really dropped `tests/auth_race.rs`"
            );
        });

        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.focused_field, SearchField::Exclude)
        });
        type_and_settle(cx, "**/store.rs");
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.exclude.as_str(), "**/store.rs");
            assert_eq!(
                app.search.include.as_str(),
                "src/**",
                "and it is a *separate* field"
            );
            assert_eq!(app.search.count_label(), "3 results in 2 files");
        });

        click(cx, "search-toggle-replace");
        app.read_with(cx, |app, _| {
            assert!(app.search.replace_open);
            assert_eq!(app.search.focused_field, SearchField::Replace);
        });
        cx.simulate_input("rotate_token");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.replace.as_str(), "rotate_token");
            assert_eq!(
                app.search.query.as_str(),
                "refresh_token",
                "typing into replace must not touch the query - four fields, four values"
            );
        });
    }

    #[gpui::test]
    fn the_query_field_has_a_real_caret_that_can_be_moved_back_into_the_text(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        cx.simulate_input("refresh_token");
        cx.run_until_parked();
        cx.simulate_keystrokes("left left left left left left");
        cx.run_until_parked();
        cx.simulate_input("ed");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.search.query.as_str(),
                "refreshed_token",
                "a real editable field, not append/backspace-only - `REVISION-2026-08-14.md` §5"
            );
        });
        assert!(
            cx.debug_bounds("search-query-caret").is_some(),
            "and it paints a real caret while focused"
        );
    }

    #[gpui::test]
    fn fold_all_really_collapses_and_expands_every_file_row(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");
        assert!(
            cx.debug_bounds("search-match-0-1-0").is_some(),
            "a match row must really paint before it can be folded away"
        );

        click(cx, "search-fold-all");
        app.read_with(cx, |app, _| assert!(app.search.all_collapsed()));
        assert!(
            cx.debug_bounds("search-match-0-1-0").is_none(),
            "collapsing must really remove the match rows, not just recolour a caret"
        );
        assert!(
            cx.debug_bounds("search-file-0").is_some(),
            "the file rows themselves stay"
        );

        click(cx, "search-fold-all");
        app.read_with(cx, |app, _| assert!(!app.search.all_collapsed()));
        assert!(cx.debug_bounds("search-match-0-1-0").is_some());
    }

    #[gpui::test]
    fn clicking_one_file_row_collapses_only_that_file(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");

        click(cx, "search-file-0");
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.collapsed.len(), 1);
            assert!(!app.search.all_collapsed());
        });
        assert!(
            cx.debug_bounds("search-fold-all").is_some(),
            "one file closed still leaves results, so fold-all still exists"
        );
    }

    #[gpui::test]
    fn a_real_in_flight_search_shows_the_searching_state_not_a_stale_or_blank_one(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);

        // First, a real completed search - so the second one below has real, different-looking
        // stale results it could wrongly keep showing if `body_state` were not actually driving
        // the render.
        type_and_settle(cx, "refresh_token");
        app.read_with(cx, |app, _| {
            assert_eq!(app.search.body_state(), BodyState::Results);
        });
        assert!(cx.debug_bounds("search-match-0-1-0").is_some());

        // A second keystroke starts a new generation. Only `simulate_input` +
        // `run_until_parked` runs here - deliberately not `type_and_settle`'s own extra
        // `advance_clock(SEARCH_DEBOUNCE * 2)` - so this really is mid-`SEARCH_DEBOUNCE`, before
        // the background walk has even started, the way every real keystroke's first frame is.
        cx.simulate_input("2");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.search.body_state(),
                BodyState::Searching,
                "a real in-flight search must report itself as searching, not silently keep the \
                 previous query's completed state"
            );
            assert_eq!(
                app.search.count_label(),
                "searching\u{2026}",
                "the count row must say so, not show the previous query's stale count"
            );
        });
        assert!(
            cx.debug_bounds("search-message").is_some(),
            "the body must really paint the searching sentence"
        );
        assert!(
            cx.debug_bounds("search-match-0-1-0").is_none(),
            "the previous query's result tree must not still be on screen while a newer, \
             different query is in flight - that would look identical to the search having \
             silently hung rather than started"
        );
    }

    #[gpui::test]
    fn clicking_a_match_row_opens_the_file_on_exactly_the_line_it_reports(cx: &mut TestAppContext) {
        let repo = TempDir::new().expect("tempdir");
        let mut content = String::new();
        for line in 1..=41 {
            content.push_str(&format!("// padding line {line}\n"));
        }
        content.push_str("let refresh_token = store.issue(&sid)?;\n"); // real line 42
        std::fs::write(repo.path().join("deep.rs"), &content).expect("write");

        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");
        assert!(
            cx.debug_bounds("search-match-0-42-0").is_some(),
            "the row must really report line 42, matching the fixture's own real line"
        );

        click(cx, "search-match-0-42-0");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.code_cursor,
                Some(42),
                "a result reported as line 42 must open on real line 42, not 41"
            );
        });
    }

    #[gpui::test]
    fn replace_all_really_rewrites_the_files_on_disk_and_reports_what_changed(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let session = repo.path().join("src/auth/session.rs");
        let readme = repo.path().join("README.md");
        let readme_before = std::fs::read_to_string(&readme).expect("read");

        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");
        click(cx, "search-toggle-replace");
        cx.simulate_input("rotate_token");
        cx.run_until_parked();

        click(cx, "search-replace-all");
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 4);
        cx.run_until_parked();

        let after = std::fs::read_to_string(&session).expect("read back");
        assert!(
            !after.contains("refresh_token"),
            "the file on disk must really have changed: {after}"
        );
        assert!(after.contains("rotate_token"));
        assert!(
            after.contains("store.issue(&sid)?;"),
            "every untouched part of the line must survive verbatim"
        );
        assert_eq!(
            std::fs::read_to_string(&readme).expect("read back"),
            readme_before,
            "a file with no matches must be byte-identical - a replace touches what it found"
        );

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.search.notice.as_deref(),
                Some("Replaced 5 matches in 4 files."),
                "\"perform real edits and report what changed\""
            );
            assert_eq!(
                app.search.body_state(),
                BodyState::NoMatch,
                "and the tree really re-ran against what the files now hold"
            );
        });
        assert!(
            cx.debug_bounds("search-notice").is_some(),
            "the report must really be on screen, not only in state"
        );
    }

    #[gpui::test]
    fn a_per_file_replace_touches_only_that_file(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        type_and_settle(cx, "refresh_token");
        click(cx, "search-toggle-replace");
        cx.simulate_input("rotate_token");
        cx.run_until_parked();

        // Which file row index 0 is, read off the real results rather than assumed - the walk
        // sorts by path, and an assumption here would make this test's subject drift with the
        // fixture.
        let first = app.read_with(cx, |app, _| {
            app.search
                .results()
                .expect("results")
                .files
                .first()
                .expect("a file")
                .path
                .clone()
        });
        let others: Vec<PathBuf> = app.read_with(cx, |app, _| {
            app.search
                .results()
                .expect("results")
                .files
                .iter()
                .skip(1)
                .map(|file| file.path.clone())
                .collect()
        });

        click(cx, "search-file-replace-0");
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 4);
        cx.run_until_parked();

        assert!(!std::fs::read_to_string(&first)
            .expect("read")
            .contains("refresh_token"));
        for other in &others {
            assert!(
                std::fs::read_to_string(other)
                    .expect("read")
                    .contains("refresh_token"),
                "a per-file replace is per file: {} was touched too",
                other.display()
            );
        }
        app.read_with(cx, |app, _| {
            assert!(app
                .search
                .notice
                .as_deref()
                .expect("a notice")
                .starts_with("Replaced"));
        });
    }

    #[gpui::test]
    fn a_file_open_in_the_editor_with_unsaved_edits_is_refused_and_named(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let session = repo.path().join("src/auth/session.rs");
        let before = std::fs::read_to_string(&session).expect("read");
        let (app, cx) = open_search(cx, &repo);

        // A real, dirty `EditBuffer` for that file - the same shape opening it in the File view
        // and typing produces.
        app.update(cx, |app, _cx| {
            let metadata = std::fs::metadata(&session).expect("the file really exists");
            let mut buffer = crate::code_surface::edit_buffer::EditBuffer::new(
                session.clone(),
                before.clone(),
                Some("rs".to_string()),
                metadata.modified().ok(),
                metadata.len(),
            );
            buffer.content.push_str("// an unsaved edit\n");
            assert!(buffer.is_dirty(), "premise: this buffer really is dirty");
            let root = app.file_tree_root.clone();
            app.insert_edit_buffer_at(root, session.clone(), buffer);
        });

        type_and_settle(cx, "refresh_token");
        click(cx, "search-toggle-replace");
        cx.simulate_input("rotate_token");
        cx.run_until_parked();
        click(cx, "search-replace-all");
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 4);
        cx.run_until_parked();

        assert_eq!(
            std::fs::read_to_string(&session).expect("read back"),
            before,
            "writing this file would destroy edits the editor still believes it owns"
        );
        app.read_with(cx, |app, _| {
            let notice = app.search.notice.as_deref().expect("a notice");
            assert!(
                notice.contains("1 file skipped"),
                "refusing silently is the failure this exists to prevent: {notice}"
            );
        });
    }

    #[gpui::test]
    fn an_invalid_regex_reports_itself_rather_than_claiming_the_worktree_is_empty(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        click(cx, "search-modifier-regex");
        type_and_settle(cx, "(unclosed");

        app.read_with(cx, |app, _| {
            assert!(matches!(
                app.search.body_state(),
                BodyState::InvalidQuery(_)
            ));
            assert_eq!(app.search.count_label(), "invalid pattern");
        });
        assert!(
            cx.debug_bounds("search-fold-all").is_none(),
            "there are no results to fold"
        );
    }

    #[gpui::test]
    fn leaving_the_search_tab_never_strands_focus_on_an_unrendered_field(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        cx.simulate_input("refresh");
        cx.run_until_parked();

        app.update_in(cx, |app, window, cx| {
            app.set_right_sidebar_view(RightSidebarView::Files, window, cx);
        });
        cx.run_until_parked();

        let (focused, query_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.search.query_focus_handle.clone())
        });
        assert_ne!(
            focused.as_ref(),
            Some(&query_handle),
            "a `FocusId` no rendered frame can resolve silently kills every context-scoped \
             binding until the next click"
        );
    }
}

/// Proves GitHub issue #162's own live-report follow-up (report (a): "when a lot of results are
/// shown it lags"). `AdeApp::render_search_body` used to build every file row and every match row
/// unconditionally, on every render - up to `crate::search::engine::MAX_MATCHES` match rows for
/// one popular query. Mirrors `crate::rail::render::rail_virtualization_tests`'s own real
/// black-box proof for the rail's identical fix (GitHub issue #364): absence/presence of a real
/// painted element, not an internal call counter, because that is the one thing a regression in
/// this exact area (an eager `.children(...)` tree standing in for real virtualization again)
/// cannot fake past.
#[cfg(test)]
mod search_virtualization_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use tempfile::TempDir;

    /// Deliberately more match rows than any plausible test viewport can show at
    /// `theme::band::SEARCH_MATCH_ROW` each - a single file with `ROW_COUNT` distinct one-per-line
    /// hits, the same "a real result tree easily holds hundreds of rows for one popular symbol"
    /// shape the live report itself described.
    const ROW_COUNT: usize = 300;

    fn fixture_repo() -> TempDir {
        let repo = TempDir::new().expect("tempdir");
        let mut content = String::new();
        for i in 0..ROW_COUNT {
            content.push_str(&format!("let needle_{i} = needle;\n"));
        }
        std::fs::write(repo.path().join("big.rs"), content).expect("write");
        repo
    }

    fn open_search<'a>(
        cx: &'a mut TestAppContext,
        repo: &TempDir,
    ) -> (gpui::Entity<AdeApp>, &'a mut gpui::VisualTestContext) {
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| app.open_search_panel(window, cx));
        cx.run_until_parked();
        (app, cx)
    }

    fn type_and_settle(cx: &mut gpui::VisualTestContext, text: &str) {
        cx.simulate_input(text);
        cx.run_until_parked();
        cx.executor().advance_clock(SEARCH_DEBOUNCE * 2);
        cx.run_until_parked();
    }

    /// The real `search-match-0-{line_number}-0` selector `render_search_match` paints under.
    fn match_selector(line_number: usize) -> &'static str {
        Box::leak(format!("search-match-0-{line_number}-0").into_boxed_str())
    }

    #[gpui::test]
    fn a_match_row_far_below_the_viewport_is_never_painted(cx: &mut TestAppContext) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        // Small enough that `ROW_COUNT` rows genuinely overflow the panel's own viewport many
        // times over.
        cx.simulate_resize(gpui::size(px(420.0), px(400.0)));
        type_and_settle(cx, "needle");

        app.read_with(cx, |app, _| {
            assert_eq!(app.search.body_state(), BodyState::Results);
        });

        let first = match_selector(1);
        let far_below = match_selector(ROW_COUNT);
        assert!(
            cx.debug_bounds(first).is_some(),
            "the first match row must really paint - if it doesn't, this test proves nothing \
             about virtualization, only that the panel is empty"
        );
        assert!(
            cx.debug_bounds(far_below).is_none(),
            "the {ROW_COUNT}th match row is far below any plausible viewport, so a real \
             virtualized list must never build it as an element at all"
        );
    }

    #[gpui::test]
    fn scrolling_the_virtualized_results_materializes_a_row_that_was_not_painted(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (app, cx) = open_search(cx, &repo);
        cx.simulate_resize(gpui::size(px(420.0), px(400.0)));
        type_and_settle(cx, "needle");

        let far_below = match_selector(ROW_COUNT);
        assert!(
            cx.debug_bounds(far_below).is_none(),
            "precondition: the last row must not be painted before scrolling"
        );

        let target_index = app.update(cx, |app, _cx| {
            let outcome = app.search.results().expect("results").clone();
            let items = state::flatten_search_list_items(&outcome, &app.search.collapsed);
            items
                .iter()
                .position(|item| {
                    matches!(
                        item,
                        state::SearchListItem::MatchRow { line_index, .. }
                            if *line_index + 1 == ROW_COUNT
                    )
                })
                .expect("the last match must be a real flattened list item")
        });
        // Several incremental calls, the same real reason `crate::rail::render::
        // rail_virtualization_tests` needs them: `gpui::ListState`'s rows this far below the fold
        // are still `Unmeasured` (contributing no height to its running total yet), so revealing
        // an item this far past the fold takes the same real incremental steps a user dragging
        // the scrollbar all the way down would.
        for _ in 0..ROW_COUNT {
            app.update(cx, |app, cx| {
                app.search_list_state.scroll_to_reveal_item(target_index);
                cx.notify();
            });
            cx.run_until_parked();
            if cx.debug_bounds(far_below).is_some() {
                break;
            }
        }

        assert!(
            cx.debug_bounds(far_below).is_some(),
            "scrolling to reveal the last row must really materialize it - if this fails the \
             list is not scrollable any more, which is a far worse regression than the render \
             cost this change set out to fix"
        );
    }

    #[gpui::test]
    fn hovering_a_visible_row_does_not_materialize_a_row_far_below_the_viewport(
        cx: &mut TestAppContext,
    ) {
        let repo = fixture_repo();
        let (_app, cx) = open_search(cx, &repo);
        cx.simulate_resize(gpui::size(px(420.0), px(400.0)));
        type_and_settle(cx, "needle");

        let first_row = match_selector(1);
        let far_below = match_selector(ROW_COUNT);
        let first_bounds = cx
            .debug_bounds(first_row)
            .expect("the first match row must really paint");
        assert!(
            cx.debug_bounds(far_below).is_none(),
            "precondition: the last row must not be painted before any hover"
        );

        cx.simulate_mouse_move(gpui::point(px(1.0), px(1.0)), None, gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_mouse_move(first_bounds.center(), None, gpui::Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds(far_below).is_none(),
            "hovering a row that is really on screen must not materialize one that is not - a \
             hover-triggered `Window::refresh()` bypassing every view's per-entity render cache is \
             exactly what made this class of list slow to hover before, and exactly what real \
             virtualization has to stay correct under"
        );
    }
}

/// The in-file find bar (`mod+F`) - `crate::search::in_file`'s model, drawn in the file view's
/// own column between its toolbar and its content.
impl AdeApp {
    /// `mod+F` - a real toggle, same idiom as [`Self::handle_toggle_palette_action`]: opens the
    /// bar over the focused file view and puts the caret in it if it is closed, closes it (same
    /// as `Escape`) if it is already open. GitHub issue #379 - the first cut only ever opened
    /// (or re-focused) it, so a second `mod+F` press could not close it the way every other
    /// toggle shortcut in this app does.
    pub(crate) fn handle_find_in_file_action(
        &mut self,
        _: &FindInFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find_bar.is_some() {
            self.close_find_bar(window, cx);
            return;
        }
        // Only over a real editable File view. There is no honest subject otherwise: the Diff
        // view is two files side by side, and a bar claiming to find "in this file" over it would
        // have to pick one silently.
        if self.active_editable_path().is_none() {
            return;
        }
        self.find_bar = Some(crate::search::in_file::FindBar::new(
            self.find_bar_focus_handle.clone(),
        ));
        self.refresh_find_bar();
        // Same handle `FindBar::new` above just cloned into `focus_handle` - no need to read it
        // back out of `self.find_bar`.
        window.focus(&self.find_bar_focus_handle, cx);
        self.reset_caret_blink(cx);
        cx.notify();
    }

    /// Closes the bar and hands focus back to the editor - the surface the user was reading, and
    /// the only one that can accept the keystroke they press next.
    pub(crate) fn close_find_bar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_bar.take().is_some() {
            let code = self.code_focus_handle.clone();
            window.focus(&code, cx);
            cx.notify();
        }
    }

    /// Re-runs the find against the buffer's **live** content - unsaved edits included, since that
    /// is what is on screen.
    pub(crate) fn refresh_find_bar(&mut self) {
        let Some(content) = self
            .active_edit_buffer()
            .map(|buffer| buffer.content.clone())
        else {
            return;
        };
        if let Some(bar) = self.find_bar.as_mut() {
            bar.recompute(&content);
        }
    }

    /// Moves the editor's caret and viewport onto the bar's current hit.
    fn reveal_current_find_hit(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_editable_path() else {
            return;
        };
        let Some(line) = self
            .find_bar
            .as_ref()
            .and_then(|bar| bar.current_hit())
            .map(|hit| hit.line_number)
        else {
            return;
        };
        self.code_cursor = Some(line);
        self.scroll_file_view_to_line(&path, line.saturating_sub(1), gpui::ScrollStrategy::Center);
        cx.notify();
    }

    /// Steps the bar one hit forward or back and reveals it.
    fn step_find_bar(&mut self, forward: bool, cx: &mut Context<Self>) {
        let moved = match self.find_bar.as_mut() {
            Some(bar) if forward => bar.step_next().is_some(),
            Some(bar) => bar.step_previous().is_some(),
            None => false,
        };
        if moved {
            self.reveal_current_find_hit(cx);
        }
    }

    /// The bar itself, or nothing when it is closed.
    pub(crate) fn render_find_bar(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let bar = self.find_bar.as_ref()?;
        let has_results = bar.has_results();
        let count = bar.count_label();
        let invalid = bar.notice().is_some();
        let bar_row = div()
            .id("find-bar")
            .debug_selector(|| "find-bar".to_string())
            .track_focus(&bar.focus_handle)
            .key_context("text-input");
        Some(
            self.wire_text_input_actions(bar_row, find_bar_query_handle(), cx)
                .on_action(cx.listener(|this, _: &TextUndo, _window, cx| {
                    if this.find_bar.as_mut().is_some_and(|bar| bar.query.undo()) {
                        this.refresh_find_bar();
                        cx.notify();
                    }
                }))
                .on_action(cx.listener(|this, _: &TextRedo, _window, cx| {
                    if this.find_bar.as_mut().is_some_and(|bar| bar.query.redo()) {
                        this.refresh_find_bar();
                        cx.notify();
                    }
                }))
                .on_key_down(cx.listener(Self::handle_find_bar_key_down))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    let Some(handle) = this.find_bar.as_ref().map(|bar| bar.focus_handle.clone())
                    else {
                        return;
                    };
                    window.focus(&handle, cx);
                    this.reset_caret_blink(cx);
                }))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(7.0))
                .h(theme::band::FILTER_ROW)
                .pl(px(12.0))
                .pr(px(8.0))
                .bg(theme::surface::HEADER)
                .border_b_1()
                .border_color(theme::border::ROW)
                .child(render_search_row_mark("/", self.ui_text_size(10.0)))
                .child(self.render_simple_input_row(
                    SimpleInput {
                        caret_selector: "find-bar-caret".into(),
                        text_selector: "find-bar-text".into(),
                        focus_handle: Some(&bar.focus_handle),
                        text: bar.query.as_str(),
                        caret_offset: bar.query.caret(),
                        selection: bar.query.selection(),
                        placeholder: "find in this file",
                        font: theme::font::MONO,
                        text_size: self.ui_text_size(11.0),
                        text_color: theme::text::SELECTED,
                        placeholder_color: theme::text::GHOST,
                        caret: widgets::SimpleInputCaret::default(),
                        field: Some(find_bar_query_handle()),
                    },
                    cx,
                ))
                .child(
                    div()
                        .id("find-bar-count")
                        .debug_selector(|| "find-bar-count".to_string())
                        .flex_none()
                        .whitespace_nowrap()
                        .font(font(theme::font::MONO))
                        .text_size(self.ui_text_size(9.5))
                        .text_color(if invalid {
                            theme::status::FAIL
                        } else {
                            theme::text::FAINTER
                        })
                        .child(count),
                )
                .children(
                    SearchModifier::ALL
                        .into_iter()
                        .map(|modifier| self.render_find_bar_modifier(modifier, cx)),
                )
                .children(
                    has_results
                        .then(|| self.render_find_bar_step(false, bar.step_tooltip(false), cx)),
                )
                .children(
                    has_results
                        .then(|| self.render_find_bar_step(true, bar.step_tooltip(true), cx)),
                )
                .child(
                    div()
                        .id("find-bar-close")
                        .debug_selector(|| "find-bar-close".to_string())
                        .flex_none()
                        .w(theme::band::ICON_BUTTON_HIT)
                        .h(theme::band::ICON_BUTTON_HIT)
                        .rounded(theme::radius::CHIP)
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
                        .tooltip(text_tooltip("Close find (Esc)"))
                        .child(
                            div()
                                .font(font(theme::font::MONO))
                                .text_size(self.ui_text_size(11.0))
                                .text_color(theme::text::FAINTER)
                                .child("\u{d7}"),
                        )
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.close_find_bar(window, cx);
                        })),
                )
                .into_any_element(),
        )
    }

    /// One `Aa` / `ab` / `.*` button on the find bar - the panel's own button, pointed at the
    /// bar's own options.
    fn render_find_bar_modifier(
        &self,
        modifier: SearchModifier,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on = self
            .find_bar
            .as_ref()
            .is_some_and(|bar| modifier.is_on(bar.options));
        let key = match modifier {
            SearchModifier::MatchCase => "case",
            SearchModifier::WholeWord => "word",
            SearchModifier::Regex => "regex",
        };
        div()
            .id(SharedString::from(format!("find-bar-modifier-{key}")))
            .debug_selector(move || format!("find-bar-modifier-{key}"))
            .flex_none()
            .w(theme::band::ICON_BUTTON_HIT)
            .h(theme::band::ICON_BUTTON_HIT)
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
                    .when(modifier == SearchModifier::WholeWord, |el| el.underline())
                    .child(modifier.label()),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if let Some(bar) = this.find_bar.as_mut() {
                    modifier.toggle(&mut bar.options);
                }
                this.refresh_find_bar();
                cx.notify();
            }))
    }

    /// One next/prev button.
    fn render_find_bar_step(
        &self,
        forward: bool,
        tooltip: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = if forward {
            "find-bar-next"
        } else {
            "find-bar-prev"
        };
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex_none()
            .w(theme::band::ICON_BUTTON_HIT)
            .h(theme::band::ICON_BUTTON_HIT)
            .rounded(theme::radius::CHIP)
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|el| el.bg(theme::surface::ROW_HOVER_ALT))
            .tooltip(text_tooltip(tooltip))
            // The file tree's own caret glyphs, rotated in meaning rather than in geometry:
            // `▾`/`▴` is down/up through a list of lines, which is exactly what stepping is.
            // Reusing the tree's family keeps this from being a fourth arrow vocabulary in a
            // window that already has three.
            .child(
                div()
                    .font(font(theme::font::MONO))
                    .text_size(self.ui_text_size(11.0))
                    .text_color(theme::text::FAINTER)
                    .child(if forward { "\u{25be}" } else { "\u{25b4}" }),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.step_find_bar(forward, cx);
            }))
    }

    /// One keystroke into the find bar.
    fn handle_find_bar_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        // `mod+F` toggles the bar closed from right inside its own field. It can't be caught by
        // the normal `FindInFile` action/keybinding dispatch the way a second press over the
        // editor is (`Self::handle_find_in_file_action`): this node deliberately sits outside the
        // `"file-editor"` context that binding is scoped to (see
        // `crate::code_surface::render`'s `render_code_surface` for the sibling-not-child docs
        // explaining why), so `secondary-f` typed into the field never reaches that binding at
        // all - it has to be caught here, the same way `Escape` already is, and closed through
        // the same `close_find_bar`.
        if keystroke.modifiers.secondary()
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.shift
            && keystroke.key == "f"
        {
            self.close_find_bar(window, cx);
            cx.stop_propagation();
            return;
        }
        // GitHub issue #336: `widgets::text_editing_modifiers` rather than a flat "any modifier
        // means not ours" - see `crate::rail::render::AdeApp::handle_filter_key_down`'s own note.
        let Some(modifiers) = widgets::text_editing_modifiers(&keystroke.key, &keystroke.modifiers)
        else {
            return;
        };
        match keystroke.key.as_str() {
            // Esc closes rather than clearing: the bar is a transient surface over a file, and
            // the field's own contents are what the user would want back if they reopen it.
            "escape" => {
                self.close_find_bar(window, cx);
                cx.stop_propagation();
                return;
            }
            "enter" => {
                self.step_find_bar(!keystroke.modifiers.shift, cx);
                cx.stop_propagation();
                return;
            }
            _ => {}
        }
        self.reset_caret_blink(cx);
        let changed = self.find_bar.as_mut().is_some_and(|bar| {
            bar.query.handle_editing_key(
                keystroke.key.as_str(),
                keystroke.key_char.as_deref(),
                modifiers,
                Instant::now(),
            )
        });
        if changed {
            self.refresh_find_bar();
            self.reveal_current_find_hit(cx);
            cx.notify();
            cx.stop_propagation();
        }
    }
}

/// The in-file find bar driven the way a user drives it: `mod+F` over a real open file, real
/// keystrokes, real next/prev, and the real editor caret really moving.
#[cfg(test)]
mod find_bar_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use tempfile::TempDir;

    const SAMPLE: &str = "let refresh_token = issue();\n\
                          if refresh_token.expired() { drop(refresh_token); }\n\
                          // nothing here\n\
                          fn refresh_token() {}\n";

    fn repo_with_open_file(
        cx: &mut TestAppContext,
    ) -> (TempDir, gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        let repo = TempDir::new().expect("tempdir");
        let file = repo.path().join("session.rs");
        std::fs::write(&file, SAMPLE).expect("write");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(file.clone(), window, cx);
        });
        cx.run_until_parked();
        (repo, app, cx)
    }

    fn click(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("`{selector}` must really paint"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    #[gpui::test]
    fn mod_f_opens_a_real_find_bar_over_the_open_file_and_focuses_it(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        assert!(
            cx.debug_bounds("find-bar").is_none(),
            "premise: the bar is closed until it is asked for"
        );

        cx.dispatch_action(FindInFile);
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("find-bar").is_some(),
            "the bar must really paint, not merely exist in state"
        );
        let (focused, handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.find_bar_focus_handle.clone())
        });
        assert_eq!(focused.as_ref(), Some(&handle));
    }

    /// `crate::default_key_bindings`' own `"secondary-f"` (GitHub issue #162), resolved the same
    /// way `crate::root::focus::tabless_window_keybinding_tests::SECONDARY_P` resolves its own
    /// binding.
    const SECONDARY_F: &str = if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    };

    #[gpui::test]
    fn mod_f_toggles_the_bar_closed_on_a_second_press(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.update(|_window, cx| cx.bind_keys(crate::default_key_bindings()));
        assert!(
            cx.debug_bounds("find-bar").is_none(),
            "premise: the bar is closed before the first press"
        );

        cx.simulate_keystrokes(SECONDARY_F);
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("find-bar").is_some(),
            "the first press must really open it"
        );
        app.read_with(cx, |app, _| assert!(app.find_bar.is_some()));

        cx.simulate_keystrokes(SECONDARY_F);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.find_bar.is_none(),
                "a second mod+F press over an already-open bar must close it - the same toggle \
                 idiom TogglePalette already uses"
            )
        });
        assert!(
            cx.debug_bounds("find-bar").is_none(),
            "closed in state but still painted is not really closed"
        );
        let (focused, code_handle) = app.update_in(cx, |app, window, cx| {
            (window.focused(cx), app.code_focus_handle.clone())
        });
        assert_eq!(
            focused.as_ref(),
            Some(&code_handle),
            "closing via the toggle must hand focus back to the editor, same as Escape"
        );
    }

    #[gpui::test]
    fn typing_really_finds_and_the_count_follows_the_panels_three_state_gate(
        cx: &mut TestAppContext,
    ) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.dispatch_action(FindInFile);
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let bar = app.find_bar.as_ref().expect("open");
            assert_eq!(
                bar.count_label(),
                "",
                "an empty field is not searched yet, which is not `no results`"
            );
        });
        assert!(
            cx.debug_bounds("find-bar-next").is_none(),
            "a control that acts on results does not exist when there are none"
        );

        cx.simulate_input("refresh_token");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let bar = app.find_bar.as_ref().expect("open");
            assert_eq!(bar.count_label(), "1 of 4");
        });
        assert!(cx.debug_bounds("find-bar-next").is_some());
        assert!(cx.debug_bounds("find-bar-prev").is_some());

        cx.simulate_input("_nonexistent");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.find_bar.as_ref().expect("open").count_label(),
                "no results"
            );
        });
        assert!(cx.debug_bounds("find-bar-next").is_none());
    }

    #[gpui::test]
    fn next_and_previous_really_move_the_editor_caret_and_wrap(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.dispatch_action(FindInFile);
        cx.run_until_parked();
        cx.simulate_input("refresh_token");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.code_cursor,
                Some(1),
                "typing lands on the first hit, on line 1"
            );
        });

        click(cx, "find-bar-next");
        app.read_with(cx, |app, _| {
            assert_eq!(app.find_bar.as_ref().expect("open").count_label(), "2 of 4");
            assert_eq!(app.code_cursor, Some(2));
        });

        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.find_bar.as_ref().expect("open").count_label(), "3 of 4");
        });

        click(cx, "find-bar-next");
        click(cx, "find-bar-next");
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.find_bar.as_ref().expect("open").count_label(),
                "1 of 4",
                "stepping past the last hit wraps"
            );
            assert_eq!(app.code_cursor, Some(1));
        });

        click(cx, "find-bar-prev");
        app.read_with(cx, |app, _| {
            assert_eq!(app.find_bar.as_ref().expect("open").count_label(), "4 of 4");
            assert_eq!(app.code_cursor, Some(4));
        });
    }

    #[gpui::test]
    fn the_match_case_button_really_changes_the_find(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.dispatch_action(FindInFile);
        cx.run_until_parked();
        cx.simulate_input("REFRESH_TOKEN");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(app.find_bar.as_ref().expect("open").count_label(), "1 of 4")
        });

        click(cx, "find-bar-modifier-case");
        app.read_with(cx, |app, _| {
            let bar = app.find_bar.as_ref().expect("open");
            assert!(bar.options.match_case);
            assert_eq!(
                bar.count_label(),
                "no results",
                "`Aa` means here exactly what it means in the panel"
            );
        });
    }

    #[gpui::test]
    fn the_find_bar_searches_the_buffer_not_the_bytes_on_disk(cx: &mut TestAppContext) {
        let (repo, app, cx) = repo_with_open_file(cx);
        let file = repo.path().join("session.rs");

        // A real unsaved edit, exactly as typing into the File view produces. `edit_buffers` is
        // keyed by the **worktree-relative** path (`AdeApp::edit_buffer_key`), which is also what
        // `active_editable_path` hands back.
        app.update(cx, |app, _cx| {
            let relative = app
                .active_editable_path()
                .expect("the File view really has an editable path");
            let buffer = app
                .edit_buffer_mut(&relative)
                .expect("the open file has a buffer");
            buffer.content.push_str("let unsaved_marker = 1;\n");
        });

        cx.dispatch_action(FindInFile);
        cx.run_until_parked();
        cx.simulate_input("unsaved_marker");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.find_bar.as_ref().expect("open").count_label(),
                "1 of 1",
                "finding the saved bytes while the user looks at their own unsaved edits would \
                 answer a question about a file that is not on screen"
            );
        });
        assert!(
            !std::fs::read_to_string(&file)
                .expect("read")
                .contains("unsaved_marker"),
            "premise: that text really is only in the buffer"
        );
    }

    #[gpui::test]
    fn escape_closes_the_bar_and_hands_focus_back_to_the_editor(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.dispatch_action(FindInFile);
        cx.run_until_parked();
        cx.simulate_input("refresh");
        cx.run_until_parked();

        cx.simulate_keystrokes("escape");
        cx.run_until_parked();

        app.read_with(cx, |app, _| assert!(app.find_bar.is_none()));
        assert!(cx.debug_bounds("find-bar").is_none());
        let (focused, code_handle, bar_handle) = app.update_in(cx, |app, window, cx| {
            (
                window.focused(cx),
                app.code_focus_handle.clone(),
                app.find_bar_focus_handle.clone(),
            )
        });
        assert_ne!(
            focused.as_ref(),
            Some(&bar_handle),
            "focus left on an unrendered node silently kills every context-scoped binding"
        );
        assert_eq!(focused.as_ref(), Some(&code_handle));
    }

    #[gpui::test]
    fn the_close_button_does_what_escape_does(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.dispatch_action(FindInFile);
        cx.run_until_parked();
        click(cx, "find-bar-close");
        app.read_with(cx, |app, _| assert!(app.find_bar.is_none()));
    }

    #[gpui::test]
    fn the_bar_has_a_real_caret_that_can_be_moved_back_into_the_query(cx: &mut TestAppContext) {
        let (_repo, app, cx) = repo_with_open_file(cx);
        cx.dispatch_action(FindInFile);
        cx.run_until_parked();
        cx.simulate_input("refresh_token");
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("find-bar-caret").is_some(),
            "a focused field paints a real caret"
        );

        cx.simulate_keystrokes("left left left left left left");
        cx.run_until_parked();
        cx.simulate_input("ed");
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.find_bar.as_ref().expect("open").query.as_str(),
                "refreshed_token"
            );
        });
    }
}
