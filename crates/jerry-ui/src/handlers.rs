//! Shared boxed-closure type aliases for component event handlers - factored out purely to
//! satisfy `clippy::type_complexity`, since the same three handler shapes repeat across
//! [`crate::components::Button`]/[`crate::components::IconButton`]/[`crate::components::ListRow`].

use gpui::{AnyView, App, ClickEvent, MouseDownEvent, Window};

pub(crate) type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
pub(crate) type RightClickHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;
pub(crate) type TooltipBuilder = Box<dyn Fn(&mut Window, &mut App) -> AnyView>;
