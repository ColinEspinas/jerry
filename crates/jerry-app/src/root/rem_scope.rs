//! A scoped GPUI rem-size override for a subtree of plain declarative elements - backs Surface
//! C's editor zoom.

use gpui::{
    div, AnyElement, App, Bounds, Div, DivFrameState, Element, ElementId, GlobalElementId, Hitbox,
    IntoElement, LayoutId, ParentElement, Pixels, StyleRefinement, Styled, Window,
};

/// An element that sets a particular rem size for its children - see the module docs.
/// `code_surface::file_view::render_file_view`/`code_surface::diff_view::render_diff_file_detail`
/// wrap their code-row content in one
/// of these, sized from the effective zoom (`code_surface::zoom::effective_code_rem_px`), so
/// `.text_size(rems(1.0))`/`.line_height(rems(1.6))` on the code rows inside scale with it, while
/// anything in the same subtree still expressed in `px()` (the gutter, the diff-sign column)
/// does not - GPUI's own `AbsoluteLength::to_pixels` split (`vendor/zed/crates/gpui/src/
/// geometry.rs`: the `Pixels` arm returns its value unchanged, the `Rems` arm multiplies by the
/// active rem size).
pub(crate) struct WithRemSize {
    div: Div,
    rem_size: Pixels,
}

impl WithRemSize {
    pub(crate) fn new(rem_size: impl Into<Pixels>) -> Self {
        Self {
            div: div(),
            rem_size: rem_size.into(),
        }
    }
}

impl Styled for WithRemSize {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl ParentElement for WithRemSize {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.div.extend(elements)
    }
}

impl Element for WithRemSize {
    type RequestLayoutState = DivFrameState;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Element::id(&self.div)
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        Element::source_location(&self.div)
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        window.with_rem_size(Some(self.rem_size), |window| {
            self.div.request_layout(id, inspector_id, window, cx)
        })
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        window.with_rem_size(Some(self.rem_size), |window| {
            self.div
                .prepaint(id, inspector_id, bounds, request_layout, window, cx)
        })
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_rem_size(Some(self.rem_size), |window| {
            self.div.paint(
                id,
                inspector_id,
                bounds,
                request_layout,
                prepaint,
                window,
                cx,
            )
        })
    }
}

impl IntoElement for WithRemSize {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}
