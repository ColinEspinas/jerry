//! A scoped GPUI rem-size override for a subtree of plain declarative elements - backs Surface
//! C's editor zoom.
//!
//! [`Window::with_rem_size`] (`vendor/zed/crates/gpui/src/window.rs`) pushes/pops a value on
//! [`Window`]'s rem-size override stack only for the dynamic extent of the closure it wraps, and
//! its own doc comment requires it be "called as part of element drawing" - it only scopes
//! anything real if the closure is where GPUI's request_layout/prepaint/paint traversal of the
//! subtree *happens*, not merely where that subtree's `div()` values get *constructed*.
//! Ordinary declarative `div().child(...)` composition in a `Render::render()` body returns its
//! element tree to the framework *before* that traversal runs, so wrapping `with_rem_size`
//! around plain construction code has no effect (`Div`'s own `request_layout`/`prepaint` in
//! `vendor/zed/crates/gpui/src/elements/div.rs` only read `window.rem_size()` when the
//! framework's traversal actually reaches that `Div`).
//!
//! The fix here is a direct, minimal port of `vendor/zed/crates/ui/src/utils/with_rem_size.rs`
//! (Zed's own `ui` crate - `crates/app` doesn't depend on it, since it has its own `crate::theme`
//! tokens instead): a small custom [`Element`] wrapping a [`Div`], whose `request_layout`/
//! `prepaint`/`paint` each call `window.with_rem_size(...)` around the corresponding `Div`
//! method call. Since a `Div`'s implementation of those methods recurses into its children's
//! same three methods synchronously, wrapping each one here scopes the override across this
//! element's entire child subtree, including nested `gpui::uniform_list` row callbacks (which lay
//! out each measured row via `layout_as_root` - itself a request_layout/prepaint/paint chain
//! reached like any other descendant while the override is still pushed).

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
