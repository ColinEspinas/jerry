//! The selected/hover row shell repeated across `crates/jerry-app/src/rail/render.rs`'s
//! `render_worktree_row`/`render_agent_row`: a reserved 2px left edge painted only while
//! selected, a selected background, and a hover background otherwise. Migrated onto this
//! component in both places (`docs/architecture/decisions.md` §27) - everything each row's own
//! content (carets, status dots, context menus, tooltips) still lives in `rail/render.rs`, wired
//! up via this component's [`ListRow::on_click`]/[`ListRow::on_right_click`]/
//! [`ListRow::tooltip`]/[`ParentElement::child`].

use gpui::{
    div, px, AnyElement, AnyView, App, ClickEvent, ElementId, Hsla, InteractiveElement,
    IntoElement, MouseButton, MouseDownEvent, ParentElement, Pixels, RenderOnce,
    StatefulInteractiveElement, Styled, Window,
};

use crate::handlers::{ClickHandler, RightClickHandler, TooltipBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRowDirection {
    Row,
    Col,
}

#[derive(IntoElement)]
pub struct ListRow {
    id: ElementId,
    direction: ListRowDirection,
    selected: bool,
    edge_color: Option<Hsla>,
    selected_bg: Option<Hsla>,
    hover_bg: Option<Hsla>,
    height: Option<Pixels>,
    padding_left: Option<Pixels>,
    padding_right: Option<Pixels>,
    padding_top: Option<Pixels>,
    padding_bottom: Option<Pixels>,
    gap: Option<Pixels>,
    children: Vec<AnyElement>,
    on_click: Option<ClickHandler>,
    on_right_click: Option<RightClickHandler>,
    tooltip: Option<TooltipBuilder>,
    debug_selector: Option<Box<dyn FnOnce() -> String>>,
}

impl ListRow {
    pub fn new(id: impl Into<ElementId>) -> Self {
        ListRow {
            id: id.into(),
            direction: ListRowDirection::Row,
            selected: false,
            edge_color: None,
            selected_bg: None,
            hover_bg: None,
            height: None,
            padding_left: None,
            padding_right: None,
            padding_top: None,
            padding_bottom: None,
            gap: None,
            children: Vec::new(),
            on_click: None,
            on_right_click: None,
            tooltip: None,
            debug_selector: None,
        }
    }

    pub fn direction(mut self, direction: ListRowDirection) -> Self {
        self.direction = direction;
        self
    }

    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// The colour painted on the reserved 2px left edge while [`Self::selected`] is `true`. Left
    /// unset, the edge stays reserved (occupies layout space) but paints nothing, matching every
    /// migrated call site's "the 2px gutter is always reserved, only its colour is conditional"
    /// rule.
    pub fn edge_color(mut self, color: Hsla) -> Self {
        self.edge_color = Some(color);
        self
    }

    pub fn selected_bg(mut self, color: Hsla) -> Self {
        self.selected_bg = Some(color);
        self
    }

    pub fn hover_bg(mut self, color: Hsla) -> Self {
        self.hover_bg = Some(color);
        self
    }

    pub fn height(mut self, height: Pixels) -> Self {
        self.height = Some(height);
        self
    }

    pub fn px(mut self, horizontal: Pixels) -> Self {
        self.padding_left = Some(horizontal);
        self.padding_right = Some(horizontal);
        self
    }

    pub fn pl(mut self, left: Pixels) -> Self {
        self.padding_left = Some(left);
        self
    }

    pub fn pr(mut self, right: Pixels) -> Self {
        self.padding_right = Some(right);
        self
    }

    pub fn pt(mut self, top: Pixels) -> Self {
        self.padding_top = Some(top);
        self
    }

    pub fn pb(mut self, bottom: Pixels) -> Self {
        self.padding_bottom = Some(bottom);
        self
    }

    pub fn gap(mut self, gap: Pixels) -> Self {
        self.gap = Some(gap);
        self
    }

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub fn on_right_click(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_right_click = Some(Box::new(handler));
        self
    }

    pub fn tooltip(
        mut self,
        build_tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        self.tooltip = Some(Box::new(build_tooltip));
        self
    }

    pub fn debug_selector(mut self, f: impl FnOnce() -> String + 'static) -> Self {
        self.debug_selector = Some(Box::new(f));
        self
    }
}

impl ParentElement for ListRow {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl RenderOnce for ListRow {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let id_for_default_selector = self.id.clone();
        let debug_selector = self.debug_selector;

        let mut element = div()
            .id(self.id)
            .debug_selector(move || match debug_selector {
                Some(f) => f(),
                None => format!("{id_for_default_selector:?}"),
            })
            .cursor_pointer()
            .w_full()
            .flex()
            .border_l(px(2.0));

        if self.direction == ListRowDirection::Col {
            element = element.flex_col();
        }
        if let Some(height) = self.height {
            element = element.h(height);
        }
        if let Some(left) = self.padding_left {
            element = element.pl(left);
        }
        if let Some(right) = self.padding_right {
            element = element.pr(right);
        }
        if let Some(top) = self.padding_top {
            element = element.pt(top);
        }
        if let Some(bottom) = self.padding_bottom {
            element = element.pb(bottom);
        }
        if let Some(gap) = self.gap {
            element = element.gap(gap);
        }

        if self.selected {
            if let Some(edge) = self.edge_color {
                element = element.border_color(edge);
            }
            if let Some(bg) = self.selected_bg {
                element = element.bg(bg);
            }
        } else if let Some(hover_bg) = self.hover_bg {
            element = element.hover(move |style| style.bg(hover_bg));
        }

        element = element.children(self.children);

        if let Some(handler) = self.on_click {
            element = element.on_click(handler);
        }
        if let Some(handler) = self.on_right_click {
            element = element.on_mouse_down(MouseButton::Right, handler);
        }
        if let Some(build_tooltip) = self.tooltip {
            element = element.tooltip(build_tooltip);
        }
        element
    }
}

#[cfg(test)]
mod tests {
    use super::{ListRow, ListRowDirection};
    use crate::theme::Theme;
    use gpui::{Context, IntoElement, ParentElement, Render, TestAppContext, Window};

    struct Harness {
        selected: bool,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let theme = Theme::default();
            let row = ListRow::new("test-row")
                .selected(self.selected)
                .edge_color(theme.colors.accent)
                .selected_bg(theme.colors.surface_selected)
                .hover_bg(theme.colors.surface_hover)
                .height(gpui::px(27.0))
                .debug_selector(|| "harness-row".to_string())
                .child("row content");
            gpui::div().child(row)
        }
    }

    #[gpui::test]
    fn a_row_paints_real_bounds_whether_selected_or_not(cx: &mut TestAppContext) {
        for selected in [false, true] {
            let (_view, cx) = cx.add_window_view(|_window, _cx| Harness { selected });
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("harness-row").is_some(),
                "selected={selected} should still paint real bounds"
            );
        }
    }

    #[gpui::test]
    fn a_click_on_the_row_reaches_the_handler(cx: &mut TestAppContext) {
        struct Clickable {
            clicked: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl Render for Clickable {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let clicked = self.clicked.clone();
                let row = ListRow::new("test-row")
                    .height(gpui::px(27.0))
                    .debug_selector(|| "harness-row".to_string())
                    .on_click(move |_event, _window, _cx| clicked.set(true))
                    .child("row content");
                gpui::div().child(row)
            }
        }

        let (view, cx) = cx.add_window_view(|_window, _cx| Clickable {
            clicked: Default::default(),
        });
        cx.run_until_parked();
        let bounds = cx.debug_bounds("harness-row").expect("row paints bounds");
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        view.read_with(cx, |harness, _| assert!(harness.clicked.get()));
    }

    #[gpui::test]
    fn a_column_row_still_paints(cx: &mut TestAppContext) {
        struct ColHarness;
        impl Render for ColHarness {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let row = ListRow::new("test-row")
                    .direction(ListRowDirection::Col)
                    .debug_selector(|| "harness-row".to_string())
                    .child("line one")
                    .child("line two");
                gpui::div().child(row)
            }
        }
        let (_view, cx) = cx.add_window_view(|_window, _cx| ColHarness);
        cx.run_until_parked();
        assert!(cx.debug_bounds("harness-row").is_some());
    }
}
