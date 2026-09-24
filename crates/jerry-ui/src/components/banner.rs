//! An info/warn/fail/ok strip with an optional trailing action - migrated from
//! `crates/jerry-app/src/rail/render.rs`'s `render_worktrees_error_banner`
//! (`docs/architecture/decisions.md` §27). Font family is left to the caller
//! ([`Banner::font_family`]) - this crate owns no font asset of its own, only `gpui::font`'s
//! plain family-name constructor.

use gpui::{
    div, font, px, AnyElement, App, ElementId, Hsla, InteractiveElement, IntoElement,
    ParentElement, RenderOnce, SharedString, Styled, Window,
};

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerVariant {
    Info,
    Warn,
    Fail,
    Ok,
}

impl BannerVariant {
    fn colors(self, theme: &Theme) -> (Hsla, Hsla) {
        let status = &theme.colors.status;
        match self {
            BannerVariant::Info => (status.info_bg, status.info),
            BannerVariant::Warn => (status.warn_bg, status.warn),
            BannerVariant::Fail => (status.fail_bg, status.fail),
            BannerVariant::Ok => (status.ok_bg, status.ok),
        }
    }
}

/// How the banner's own edge is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerShape {
    /// Rounded, bordered on all sides in the variant's own colour - the generic default.
    Card,
    /// Square, bordered on the bottom edge only, in `theme.colors.border` rather than the
    /// variant colour - the exact shape `render_worktrees_error_banner` hand-rolled: a standing
    /// strip directly under the rail's filter row, not a floating card.
    Strip,
}

#[derive(IntoElement)]
pub struct Banner {
    id: ElementId,
    variant: BannerVariant,
    shape: BannerShape,
    message: SharedString,
    theme: Theme,
    font_family: Option<SharedString>,
    action: Option<AnyElement>,
    debug_selector: Option<Box<dyn FnOnce() -> String>>,
}

impl Banner {
    pub fn new(
        id: impl Into<ElementId>,
        variant: BannerVariant,
        message: impl Into<SharedString>,
        theme: Theme,
    ) -> Self {
        Banner {
            id: id.into(),
            variant,
            shape: BannerShape::Card,
            message: message.into(),
            theme,
            font_family: None,
            action: None,
            debug_selector: None,
        }
    }

    pub fn shape(mut self, shape: BannerShape) -> Self {
        self.shape = shape;
        self
    }

    pub fn font_family(mut self, family: impl Into<SharedString>) -> Self {
        self.font_family = Some(family.into());
        self
    }

    pub fn action(mut self, action: impl IntoElement) -> Self {
        self.action = Some(action.into_any_element());
        self
    }

    pub fn debug_selector(mut self, f: impl FnOnce() -> String + 'static) -> Self {
        self.debug_selector = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Banner {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (bg, fg) = self.variant.colors(&self.theme);
        let id_for_default_selector = self.id.clone();
        let debug_selector = self.debug_selector;

        let mut element = div()
            .id(self.id)
            .debug_selector(move || match debug_selector {
                Some(f) => f(),
                None => format!("{id_for_default_selector:?}"),
            })
            .flex_none()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(self.theme.spacing.sm)
            .px(self.theme.spacing.lg)
            .py(self.theme.spacing.sm)
            .bg(bg)
            .text_size(px(10.0))
            .text_color(fg);

        element = match self.shape {
            BannerShape::Card => element
                .rounded(self.theme.radius.lg)
                .border_1()
                .border_color(fg),
            BannerShape::Strip => element.border_b_1().border_color(self.theme.colors.border),
        };
        if let Some(family) = self.font_family {
            element = element.font(font(family));
        }

        element = element.child(div().flex_1().min_w_0().child(self.message));
        if let Some(action) = self.action {
            element = element.child(action);
        }
        element
    }
}

#[cfg(test)]
mod tests {
    use super::{Banner, BannerShape, BannerVariant};
    use crate::theme::Theme;
    use gpui::{
        Context, InteractiveElement, IntoElement, ParentElement, Render, TestAppContext, Window,
    };

    struct Harness {
        variant: BannerVariant,
        shape: BannerShape,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let banner = Banner::new(
                "test-banner",
                self.variant,
                "something went wrong",
                Theme::default(),
            )
            .shape(self.shape)
            .debug_selector(|| "harness-banner".to_string());
            gpui::div().child(banner)
        }
    }

    #[gpui::test]
    fn every_variant_and_shape_paints_a_real_debug_bounds_entry(cx: &mut TestAppContext) {
        for variant in [
            BannerVariant::Info,
            BannerVariant::Warn,
            BannerVariant::Fail,
            BannerVariant::Ok,
        ] {
            for shape in [BannerShape::Card, BannerShape::Strip] {
                let (_view, cx) = cx.add_window_view(|_window, _cx| Harness { variant, shape });
                cx.run_until_parked();
                assert!(
                    cx.debug_bounds("harness-banner").is_some(),
                    "{variant:?}/{shape:?} should paint real bounds"
                );
            }
        }
    }

    #[gpui::test]
    fn an_action_child_is_painted_alongside_the_message(cx: &mut TestAppContext) {
        struct WithAction;
        impl Render for WithAction {
            fn render(
                &mut self,
                _window: &mut Window,
                _cx: &mut Context<Self>,
            ) -> impl IntoElement {
                let banner = Banner::new(
                    "test-banner",
                    BannerVariant::Fail,
                    "failed to list worktrees",
                    Theme::default(),
                )
                .action(
                    gpui::div()
                        .id("banner-action")
                        .debug_selector(|| "harness-banner-action".to_string())
                        .child("Dismiss"),
                );
                gpui::div().child(banner)
            }
        }
        let (_view, cx) = cx.add_window_view(|_window, _cx| WithAction);
        cx.run_until_parked();
        assert!(cx.debug_bounds("harness-banner-action").is_some());
    }
}
