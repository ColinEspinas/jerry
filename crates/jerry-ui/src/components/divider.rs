//! A 1px rule, in `theme.colors.border`. Not yet migrated onto a `jerry-app` call site - see
//! `docs/architecture/decisions.md` §27's follow-up list.

use gpui::{div, px, App, ElementId, InteractiveElement, IntoElement, RenderOnce, Styled, Window};

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DividerOrientation {
    Horizontal,
    Vertical,
}

#[derive(IntoElement)]
pub struct Divider {
    id: ElementId,
    orientation: DividerOrientation,
    theme: Theme,
    debug_selector: Option<Box<dyn FnOnce() -> String>>,
}

impl Divider {
    pub fn new(id: impl Into<ElementId>, theme: Theme) -> Self {
        Divider {
            id: id.into(),
            orientation: DividerOrientation::Horizontal,
            theme,
            debug_selector: None,
        }
    }

    pub fn vertical(mut self) -> Self {
        self.orientation = DividerOrientation::Vertical;
        self
    }

    pub fn debug_selector(mut self, f: impl FnOnce() -> String + 'static) -> Self {
        self.debug_selector = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Divider {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let id_for_default_selector = self.id.clone();
        let debug_selector = self.debug_selector;

        let element = div()
            .id(self.id)
            .debug_selector(move || match debug_selector {
                Some(f) => f(),
                None => format!("{id_for_default_selector:?}"),
            })
            .flex_none()
            .bg(self.theme.colors.border);

        match self.orientation {
            DividerOrientation::Horizontal => element.w_full().h(px(1.0)),
            DividerOrientation::Vertical => element.h_full().w(px(1.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Divider;
    use crate::theme::Theme;
    use gpui::{Context, IntoElement, ParentElement, Render, TestAppContext, Window};

    struct Harness {
        vertical: bool,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let mut divider = Divider::new("test-divider", Theme::default())
                .debug_selector(|| "harness-divider".to_string());
            if self.vertical {
                divider = divider.vertical();
            }
            gpui::div().child(divider)
        }
    }

    #[gpui::test]
    fn both_orientations_paint_a_real_debug_bounds_entry(cx: &mut TestAppContext) {
        for vertical in [false, true] {
            let (_view, cx) = cx.add_window_view(|_window, _cx| Harness { vertical });
            cx.run_until_parked();
            assert!(cx.debug_bounds("harness-divider").is_some());
        }
    }
}
