//! A small rounded label chip - status/kind tags (`A`/`M`/`D` change letters, agent-kind chips,
//! and the like). Not yet migrated onto a `jerry-app` call site - see
//! `docs/architecture/decisions.md` §27's follow-up list.

use gpui::{
    div, px, App, ElementId, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, Styled, Window,
};

use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeTone {
    Neutral,
    Info,
    Warn,
    Fail,
    Ok,
}

impl BadgeTone {
    fn colors(self, theme: &Theme) -> (Hsla, Hsla) {
        let c = &theme.colors;
        match self {
            BadgeTone::Neutral => (c.surface_raised, c.text_muted),
            BadgeTone::Info => (c.status.info_bg, c.status.info),
            BadgeTone::Warn => (c.status.warn_bg, c.status.warn),
            BadgeTone::Fail => (c.status.fail_bg, c.status.fail),
            BadgeTone::Ok => (c.status.ok_bg, c.status.ok),
        }
    }
}

#[derive(IntoElement)]
pub struct Badge {
    id: ElementId,
    label: SharedString,
    tone: BadgeTone,
    theme: Theme,
    debug_selector: Option<Box<dyn FnOnce() -> String>>,
}

impl Badge {
    pub fn new(
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        tone: BadgeTone,
        theme: Theme,
    ) -> Self {
        Badge {
            id: id.into(),
            label: label.into(),
            tone,
            theme,
            debug_selector: None,
        }
    }

    pub fn debug_selector(mut self, f: impl FnOnce() -> String + 'static) -> Self {
        self.debug_selector = Some(Box::new(f));
        self
    }
}

impl RenderOnce for Badge {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let (bg, fg) = self.tone.colors(&self.theme);
        let id_for_default_selector = self.id.clone();
        let debug_selector = self.debug_selector;

        div()
            .id(self.id)
            .debug_selector(move || match debug_selector {
                Some(f) => f(),
                None => format!("{id_for_default_selector:?}"),
            })
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .px(self.theme.spacing.xs)
            .rounded(self.theme.radius.pill)
            .bg(bg)
            .text_size(px(9.5))
            .text_color(fg)
            .child(self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::{Badge, BadgeTone};
    use crate::theme::Theme;
    use gpui::{Context, IntoElement, ParentElement, Render, TestAppContext, Window};

    struct Harness(BadgeTone);

    impl Render for Harness {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let badge = Badge::new("test-badge", "M", self.0, Theme::default())
                .debug_selector(|| "harness-badge".to_string());
            gpui::div().child(badge)
        }
    }

    #[gpui::test]
    fn every_tone_paints_a_real_debug_bounds_entry(cx: &mut TestAppContext) {
        for tone in [
            BadgeTone::Neutral,
            BadgeTone::Info,
            BadgeTone::Warn,
            BadgeTone::Fail,
            BadgeTone::Ok,
        ] {
            let (_view, cx) = cx.add_window_view(|_window, _cx| Harness(tone));
            cx.run_until_parked();
            assert!(
                cx.debug_bounds("harness-badge").is_some(),
                "{tone:?} should paint"
            );
        }
    }
}
