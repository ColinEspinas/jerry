//! A real, scoped GPUI rem-size override for a subtree of plain declarative elements - backs
//! Surface C's real editor zoom (`design_handoff_jerry_ade/revision/CHANGELOG.md`'s 2026-07-29
//! entry, change 6).
//!
//! [`Window::set_rem_size`]/[`Window::with_rem_size`] (`vendor/zed/crates/gpui/src/window.rs`)
//! are the real, verified GPUI mechanism for this, but `with_rem_size`'s own doc comment says
//! "This method must only be called as part of element drawing" - it pushes/pops a value on
//! [`Window`]'s own rem-size override stack for exactly the dynamic extent of the closure it
//! wraps, which only actually scopes anything real if that closure is where GPUI's own
//! request_layout/prepaint/paint traversal of the affected subtree *happens*, not merely where
//! that subtree's `div()` values get *constructed* (construction is plain, lazy Rust struct
//! building - no layout happens until a later, separate traversal reaches it). Ordinary
//! declarative `div().child(...)` composition in a `Render::render()` body returns its element
//! tree to the framework *before* that traversal ever runs, so wrapping `with_rem_size` around
//! plain construction code has no real effect - confirmed by reading `Div`'s own
//! `request_layout`/`prepaint` (`vendor/zed/crates/gpui/src/elements/div.rs`), which read
//! `window.rem_size()` (and recurse into children) only when the framework's traversal actually
//! reaches that specific `Div`, not at construction time.
//!
//! The real, verified fix - copied from `vendor/zed/crates/ui/src/utils/with_rem_size.rs`
//! (Zed's own `ui` crate, not invented here) rather than guessed: a small custom [`Element`]
//! wrapping a [`Div`], whose own `request_layout`/`prepaint`/`paint` each call
//! `window.with_rem_size(...)` around the *actual* corresponding `Div` method call. Since those
//! three methods are exactly the points the framework's traversal calls when it reaches this
//! node - and a `Div`'s own implementation of those methods recurses into its children's same
//! three methods synchronously, within the same call - wrapping each one here really does scope
//! the override across this element's entire real child subtree, including nested
//! `gpui::uniform_list` row-measurement/row-render callbacks (verified against
//! `vendor/zed/crates/gpui/src/elements/uniform_list.rs`'s `measure_item`, which lays out the
//! measured row via `layout_as_root` - itself just a request_layout/prepaint/paint call chain,
//! so it's reached exactly like any other descendant while the override is still pushed).
//!
//! `crates/app` doesn't depend on `vendor/zed/crates/ui` (it has its own `crate::theme` design
//! tokens rather than that crate's, per this codebase's established convention - see
//! `crate::theme`'s own module docs), so this is a direct, minimal port of that one real type,
//! not a new invented pattern.

use gpui::{
    div, AnyElement, App, Bounds, Div, DivFrameState, Element, ElementId, GlobalElementId, Hitbox,
    IntoElement, LayoutId, ParentElement, Pixels, StyleRefinement, Styled, Window,
};

/// An element that sets a particular real rem size for its children - see the module docs.
/// `code_surface::render_file_view`/`render_diff_file_detail` wrap their code-row content in
/// one of these, sized from the real, effective zoom (`code_surface::effective_code_rem_px`),
/// so `.text_size(rems(1.0))`/`.line_height(rems(1.6))` on the code rows inside actually scale
/// with it, while anything in the same subtree still expressed in `px()` (the gutter, the
/// diff-sign column) does not - GPUI's own real `AbsoluteLength::to_pixels` split (`vendor/zed/
/// crates/gpui/src/geometry.rs`: the `Pixels` arm returns its value unchanged, the `Rems` arm
/// multiplies by the active rem size), not a second, hand-rolled scaling scheme.
pub(super) struct WithRemSize {
    div: Div,
    rem_size: Pixels,
}

impl WithRemSize {
    pub(super) fn new(rem_size: impl Into<Pixels>) -> Self {
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
