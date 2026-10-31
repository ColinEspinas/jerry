//! A small bordered/filled/text button - the shape
//! `crates/jerry-app/src/settings/render.rs`'s `render_theme_action_button` and
//! `crates/jerry-app/src/settings/widgets.rs`'s "Open file" button both hand-rolled identically
//! (`h(20px)`, `px(8px)`, `radius::BUTTON`, `MEDIUM` `10.5px` label) before migrating onto this
//! component - see `docs/architecture/decisions.md` §27.

use gpui::{
    div, px, AnyElement, App, ClickEvent, ElementId, FontWeight, Hsla, InteractiveElement,
    IntoElement, ParentElement, RenderOnce, SharedString, StatefulInteractiveElement, Styled,
    Window,
};

use crate::handlers::ClickHandler;
use crate::state_style::{ElementState, StateStyle, StyleSet};
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonVariant {
    /// Solid `accent` fill, `text_on_accent` label - a primary call to action. Not yet used by
    /// any migrated `jerry-app` call site (none of the two migrated buttons needed one); real
    /// and tested, but visually unverified against a live screen.
    Primary,
    /// Bordered, transparent fill - the two migrated call sites' real shape.
    Secondary,
    /// No border, no resting fill - hover-only feedback.
    Ghost,
}

impl ButtonVariant {
    fn state_style(self, theme: &Theme) -> StateStyle {
        let c = &theme.colors;
        match self {
            ButtonVariant::Primary => {
                StateStyle::new(StyleSet::default().bg(c.accent).text(c.text_on_accent))
                    .hover(StyleSet::default().bg(c.accent_hover))
                    .disabled(StyleSet::default().bg(c.surface).text(c.text_disabled))
            }
            ButtonVariant::Secondary => {
                StateStyle::new(StyleSet::default().border(c.border).text(c.text_muted))
                    .hover(StyleSet::default().bg(c.surface_hover))
                    .disabled(
                        StyleSet::default()
                            .border(c.border_disabled)
                            .text(c.text_disabled),
                    )
            }
            ButtonVariant::Ghost => StateStyle::new(StyleSet::default().text(c.text_muted))
                .hover(StyleSet::default().bg(c.surface_hover))
                .disabled(StyleSet::default().text(c.text_disabled)),
        }
    }
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    theme: Theme,
    disabled: bool,
    icon: Option<AnyElement>,
    text_color: Option<Hsla>,
    border_color: Option<Hsla>,
    hover_bg: Option<Hsla>,
    font_family: Option<SharedString>,
    debug_selector: Option<Box<dyn FnOnce() -> String>>,
    on_click: Option<ClickHandler>,
}

impl Button {
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>, theme: Theme) -> Self {
        Button {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::Secondary,
            theme,
            disabled: false,
            icon: None,
            text_color: None,
            border_color: None,
            hover_bg: None,
            font_family: None,
            debug_selector: None,
            on_click: None,
        }
    }

    pub fn primary(id: impl Into<ElementId>, label: impl Into<SharedString>, theme: Theme) -> Self {
        Self::new(id, label, theme).variant(ButtonVariant::Primary)
    }

    pub fn ghost(id: impl Into<ElementId>, label: impl Into<SharedString>, theme: Theme) -> Self {
        Self::new(id, label, theme).variant(ButtonVariant::Ghost)
    }

    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    /// Overrides the variant's own resting text colour - a migrated call site with its own
    /// status-tinted button (e.g. a fail-red "Restart sessions" action) passes it here rather
    /// than losing that tint to the generic variant default.
    pub fn text_color(mut self, color: Hsla) -> Self {
        self.text_color = Some(color);
        self
    }

    /// Overrides the [`ButtonVariant::Secondary`] border colour - see [`Self::text_color`].
    pub fn border_color(mut self, color: Hsla) -> Self {
        self.border_color = Some(color);
        self
    }

    /// Overrides the hover background - see [`Self::text_color`].
    pub fn hover_bg(mut self, color: Hsla) -> Self {
        self.hover_bg = Some(color);
        self
    }

    /// Sets the label's font family - this crate owns no font asset of its own, only `gpui::
    /// font`'s plain family-name constructor, matching [`crate::components::Banner::
    /// font_family`].
    pub fn font_family(mut self, family: impl Into<SharedString>) -> Self {
        self.font_family = Some(family.into());
        self
    }

    /// Overrides the default `{id:?}`-derived `debug_selector` - a migrated call site passes its
    /// existing lookup string here so a test written against it keeps working unchanged.
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

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut style = self.variant.state_style(&self.theme);
        if let Some(text) = self.text_color {
            style.rest.text = Some(text);
        }
        if let Some(border) = self.border_color {
            style.rest.border = Some(border);
        }
        if let Some(hover_bg) = self.hover_bg {
            style.hover = Some(style.hover.unwrap_or_default().bg(hover_bg));
        }
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
            .flex()
            .items_center()
            .justify_center()
            .gap(self.theme.spacing.xs)
            .h(px(20.0))
            .px(self.theme.spacing.md)
            .rounded(self.theme.radius.md)
            .border_1()
            .font_weight(FontWeight::MEDIUM)
            .text_size(px(10.5));
        element = style.apply(element, state);
        if let Some(family) = self.font_family {
            element = element.font(gpui::font(family));
        }

        if self.disabled {
            element = element.cursor_default();
        } else {
            element = element.cursor_pointer();
            if let Some(handler) = self.on_click {
                element = element.on_click(handler);
            }
        }

        if let Some(icon) = self.icon {
            element = element.child(icon);
        }
        element.child(self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::{Button, ButtonVariant};
    use crate::theme::Theme;
    use gpui::{Context, IntoElement, ParentElement, Render, TestAppContext, Window};

    /// Rebuilds a fresh `Button` from plain data on every real render call - `RenderOnce`
    /// consumes its component by value, so a `Render`-implementing test host can never hold onto
    /// one across repaints, only the data to build one.
    struct Harness {
        variant: ButtonVariant,
        disabled: bool,
        clicked: std::rc::Rc<std::cell::Cell<bool>>,
    }

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let clicked = self.clicked.clone();
            let button = Button::new("test-button", "Label", Theme::default())
                .variant(self.variant)
                .disabled(self.disabled)
                .debug_selector(|| "harness-button".to_string())
                .on_click(move |_event, _window, _cx| clicked.set(true));
            gpui::div().child(button)
        }
    }

    #[gpui::test]
    fn every_variant_paints_a_real_debug_bounds_entry(cx: &mut TestAppContext) {
        for variant in [
            ButtonVariant::Primary,
            ButtonVariant::Secondary,
            ButtonVariant::Ghost,
        ] {
            let (_view, cx) = cx.add_window_view(|_window, _cx| Harness {
                variant,
                disabled: false,
                clicked: Default::default(),
            });
            cx.run_until_parked();
            let bounds = cx.debug_bounds("harness-button");
            assert!(bounds.is_some(), "{variant:?} should paint real bounds");
        }
    }

    #[gpui::test]
    fn a_disabled_button_still_paints_but_ignores_a_click(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| Harness {
            variant: ButtonVariant::Secondary,
            disabled: true,
            clicked: Default::default(),
        });
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("harness-button")
            .expect("a disabled button still paints");

        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        view.read_with(cx, |harness, _| {
            assert!(
                !harness.clicked.get(),
                "a disabled button must drop its on_click handler, not merely ignore the click"
            );
        });
    }

    #[gpui::test]
    fn an_enabled_button_click_reaches_the_handler(cx: &mut TestAppContext) {
        let (view, cx) = cx.add_window_view(|_window, _cx| Harness {
            variant: ButtonVariant::Secondary,
            disabled: false,
            clicked: Default::default(),
        });
        cx.run_until_parked();
        let bounds = cx
            .debug_bounds("harness-button")
            .expect("an enabled button paints");

        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();

        view.read_with(cx, |harness, _| {
            assert!(harness.clicked.get(), "the click must reach on_click");
        });
    }
}
