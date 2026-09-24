//! A declarative `{ rest, hover, active, focused, disabled, selected }` state map, so a
//! component describes its own look once instead of hand-writing a `.hover(|el| el.bg(..))`
//! chain at every call site. Modelled on gpui-kit's state-style pattern (see
//! `docs/architecture/decisions.md` §27) - no gpui-kit code, this crate depends on `gpui` alone.

use gpui::{Hsla, StatefulInteractiveElement, Styled};

/// The subset of a style any one state actually overrides. `None` on a field means "leave
/// whatever the previous state already set" - a hover state that only changes the background
/// doesn't have to repeat the resting text colour.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StyleSet {
    pub bg: Option<Hsla>,
    pub text: Option<Hsla>,
    pub border: Option<Hsla>,
}

impl StyleSet {
    pub fn bg(mut self, color: Hsla) -> Self {
        self.bg = Some(color);
        self
    }

    pub fn text(mut self, color: Hsla) -> Self {
        self.text = Some(color);
        self
    }

    pub fn border(mut self, color: Hsla) -> Self {
        self.border = Some(color);
        self
    }
}

/// Which of the non-`rest` states currently apply. Hover and active/press are states GPUI
/// itself tracks per frame (`Styled::hover`/`StatefulInteractiveElement::active`, both real
/// interaction-driven style closures) - [`StateStyle::apply`] wires those directly. Selection,
/// disablement and focus are not something GPUI infers on its own; the caller already knows
/// them (they come from application state, not the pointer), so it states them here instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ElementState {
    pub disabled: bool,
    pub selected: bool,
    pub focused: bool,
}

/// A component's full `{ rest, hover, active, focused, disabled, selected }` map. Every state
/// past `rest` is optional - a component that never needs a press state simply never calls
/// [`Self::active`].
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StateStyle {
    pub rest: StyleSet,
    pub hover: Option<StyleSet>,
    pub active: Option<StyleSet>,
    pub focused: Option<StyleSet>,
    pub disabled: Option<StyleSet>,
    pub selected: Option<StyleSet>,
}

impl StateStyle {
    pub fn new(rest: StyleSet) -> Self {
        StateStyle {
            rest,
            ..Default::default()
        }
    }

    pub fn hover(mut self, style: StyleSet) -> Self {
        self.hover = Some(style);
        self
    }

    pub fn active(mut self, style: StyleSet) -> Self {
        self.active = Some(style);
        self
    }

    pub fn focused(mut self, style: StyleSet) -> Self {
        self.focused = Some(style);
        self
    }

    pub fn disabled(mut self, style: StyleSet) -> Self {
        self.disabled = Some(style);
        self
    }

    pub fn selected(mut self, style: StyleSet) -> Self {
        self.selected = Some(style);
        self
    }

    /// Paints `rest`, then layers on `focused`/`selected`/`disabled` (in that priority order, so
    /// a disabled+selected element reads as disabled) if `state` says they apply, then arms real
    /// GPUI `hover`/`active` style closures for [`Self::hover`]/[`Self::active`] regardless of
    /// `state` - those two are live pointer interactions, not something the caller pre-decides.
    pub fn apply<E>(&self, element: E, state: ElementState) -> E
    where
        E: Styled + StatefulInteractiveElement,
    {
        let mut element = apply_style_set(element, self.rest);
        if state.focused {
            if let Some(style) = self.focused {
                element = apply_style_set(element, style);
            }
        }
        if state.selected {
            if let Some(style) = self.selected {
                element = apply_style_set(element, style);
            }
        }
        if state.disabled {
            if let Some(style) = self.disabled {
                element = apply_style_set(element, style);
            }
        }
        if let Some(hover) = self.hover {
            element =
                element.hover(move |style_refinement| apply_style_set(style_refinement, hover));
        }
        if let Some(active) = self.active {
            element =
                element.active(move |style_refinement| apply_style_set(style_refinement, active));
        }
        element
    }
}

fn apply_style_set<E: Styled>(mut element: E, set: StyleSet) -> E {
    if let Some(bg) = set.bg {
        element = element.bg(bg);
    }
    if let Some(text) = set.text {
        element = element.text_color(text);
    }
    if let Some(border) = set.border {
        element = element.border_color(border);
    }
    element
}

#[cfg(test)]
mod tests {
    use super::{ElementState, StateStyle, StyleSet};
    use gpui::Hsla;

    fn gray(l: f32) -> Hsla {
        Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        }
    }

    #[test]
    fn builder_methods_set_exactly_the_state_they_name() {
        let rest = StyleSet::default().bg(gray(0.1));
        let style = StateStyle::new(rest)
            .hover(StyleSet::default().bg(gray(0.2)))
            .selected(StyleSet::default().border(gray(0.3)))
            .disabled(StyleSet::default().text(gray(0.4)));

        assert_eq!(style.rest, rest);
        assert_eq!(style.hover.expect("hover set").bg, Some(gray(0.2)));
        assert_eq!(
            style.selected.expect("selected set").border,
            Some(gray(0.3))
        );
        assert_eq!(style.disabled.expect("disabled set").text, Some(gray(0.4)));
        assert!(style.active.is_none());
        assert!(style.focused.is_none());
    }

    #[test]
    fn a_default_element_state_has_no_state_active() {
        assert_eq!(
            ElementState::default(),
            ElementState {
                disabled: false,
                selected: false,
                focused: false,
            }
        );
    }
}
