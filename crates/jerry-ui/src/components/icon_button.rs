//! A square, icon-only button - the hit-box shape
//! `crates/jerry-app/src/theme.rs`'s own `band::ICON_BUTTON_HIT` (17px) already names as shared
//! by several hand-rolled icon-only controls in that crate. Not yet migrated onto any of them
//! (see `docs/architecture/decisions.md` §27's follow-up list) - built and tested for the
//! components that do adopt it next.

use gpui::{
    div, px, AnyElement, AnyView, App, ClickEvent, ElementId, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, StatefulInteractiveElement, Styled, Window,
};

use crate::handlers::{ClickHandler, TooltipBuilder};
use crate::state_style::{ElementState, StateStyle, StyleSet};
use crate::theme::Theme;

#[derive(IntoElement)]
pub struct IconButton {
    id: ElementId,
    icon: AnyElement,
    theme: Theme,
    /// Mirrors `theme::band::ICON_BUTTON_HIT` (`17px`) by default.
    size: gpui::Pixels,
    disabled: bool,
    tooltip: Option<TooltipBuilder>,
    debug_selector: Option<Box<dyn FnOnce() -> String>>,
    on_click: Option<ClickHandler>,
}

impl IconButton {
    pub fn new(id: impl Into<ElementId>, icon: impl IntoElement, theme: Theme) -> Self {
        IconButton {
            id: id.into(),
            icon: icon.into_any_element(),
            theme,
            size: px(17.0),
            disabled: false,
            tooltip: None,
            debug_selector: None,
            on_click: None,
        }
    }

    pub fn size(mut self, size: gpui::Pixels) -> Self {
        self.size = size;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
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

    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for IconButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let c = &self.theme.colors;
        let style = StateStyle::new(StyleSet::default().text(c.text_muted))
            .hover(StyleSet::default().bg(c.surface_hover).text(c.text))
            .disabled(StyleSet::default().text(c.text_disabled));
        let state = ElementState {
            disabled: self.disabled,
            ..Default::default()
        };
        let id_for_default_selector = self.id.clone();
        let debug_selector = self.debug_selector;

        let mut element = div()
            .id(self.id)
            .debug_selector(move || match debug_selector {
                Some(f) => f(),
                None => format!("{id_for_default_selector:?}"),
            })
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .w(self.size)
            .h(self.size)
            .rounded(self.theme.radius.sm)
            .child(self.icon);
        element = style.apply(element, state);

        if self.disabled {
            element = element.cursor_default();
        } else {
            element = element.cursor_pointer();
            if let Some(handler) = self.on_click {
                element = element.on_click(handler);
            }
        }
        if let Some(build_tooltip) = self.tooltip {
            element = element.tooltip(build_tooltip);
        }
        element
    }
}

#[cfg(test)]
mod tests {
    use super::IconButton;
    use crate::theme::Theme;
    use gpui::{Context, IntoElement, ParentElement, Render, TestAppContext, Window};

    struct Harness;

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let button = IconButton::new("icon-button", "x", Theme::default())
                .debug_selector(|| "harness-icon-button".to_string());
            gpui::div().child(button)
        }
    }

    #[gpui::test]
    fn an_icon_button_paints_a_real_debug_bounds_entry(cx: &mut TestAppContext) {
        let (_view, cx) = cx.add_window_view(|_window, _cx| Harness);
        cx.run_until_parked();
        assert!(cx.debug_bounds("harness-icon-button").is_some());
    }
}
