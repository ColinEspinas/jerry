//! The file tree's right-click context menu, as pure data (GitHub issue #19 §1): which actions
//! a given target offers, and where the popover has to be drawn so it stays inside the window.
//!
//! GPUI-free on purpose, exactly like [`crate::sidebar::file_tree`] beside it - "does a folder
//! offer Collapse Subtree" and "does a menu opened 3px from the bottom edge flip upwards" are
//! both decisions that can be tested without a window, and [`crate::sidebar::render`] only
//! *draws* what this module decides.
//!
//! Zed's own `ui::ContextMenu` (`vendor/zed/crates/ui/src/components/context_menu.rs`) is not
//! reachable from here: it lives in Zed's `ui` crate, not in `gpui`, and this workspace
//! deliberately depends on `gpui`/`gpui_platform` only. The popover is therefore built from the
//! same real scrim + absolutely-positioned panel shape
//! `crate::work_surface::render::AdeApp::render_plus_menu` already established in this app.

use std::path::{Path, PathBuf};

/// What a right-click landed on. `Empty` is a real target, not a fallback: the area below the
/// last row offers its own menu (New File / New Folder / Paste / Collapse All), scoped to the
/// worktree root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTarget {
    File(PathBuf),
    Folder(PathBuf),
    Empty,
}

impl ContextTarget {
    /// The path this target's actions operate *on*, or `None` for the empty area.
    pub fn path(&self) -> Option<&Path> {
        match self {
            ContextTarget::File(path) | ContextTarget::Folder(path) => Some(path),
            ContextTarget::Empty => None,
        }
    }

    /// The directory a "New File"/"New Folder"/"Paste" from this target should land in: the
    /// folder itself, a file's parent, or the tree root for the empty area.
    pub fn destination_dir<'a>(&'a self, root: &'a Path) -> &'a Path {
        match self {
            ContextTarget::Folder(path) => path,
            ContextTarget::File(path) => path.parent().unwrap_or(root),
            ContextTarget::Empty => root,
        }
    }
}

/// One menu row's real action. Every variant is wired to a real handler in
/// [`crate::sidebar::tree_ops`]; there is no decorative entry here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    Open,
    NewFile,
    NewFolder,
    Rename,
    Duplicate,
    Cut,
    Copy,
    Paste,
    CopyPath,
    CopyRelativePath,
    CollapseSubtree,
    CollapseAll,
    Delete,
    Reveal,
}

impl MenuAction {
    /// The row's label. `Reveal`'s wording is deliberately generic - this app hands the path to
    /// the OS default-open handler (`xdg-open`/`open`/`cmd /c start`), which is whatever file
    /// manager the user has configured, not a specific named one.
    pub fn label(self) -> &'static str {
        match self {
            MenuAction::Open => "Open",
            MenuAction::NewFile => "New File",
            MenuAction::NewFolder => "New Folder",
            MenuAction::Rename => "Rename",
            MenuAction::Duplicate => "Duplicate",
            MenuAction::Cut => "Cut",
            MenuAction::Copy => "Copy",
            MenuAction::Paste => "Paste",
            MenuAction::CopyPath => "Copy Path",
            MenuAction::CopyRelativePath => "Copy Relative Path",
            MenuAction::CollapseSubtree => "Collapse Subtree",
            MenuAction::CollapseAll => "Collapse All",
            MenuAction::Delete => "Delete",
            MenuAction::Reveal => "Reveal in file manager",
        }
    }

    /// The keystroke this action *also* has a real binding for while the tree is focused
    /// (`crate::default_key_bindings`), in `crate::keymap::resolve_combo`'s own spec syntax -
    /// `None` for the ones that are menu-only. Deliberately not a hard-coded keycap string: the
    /// keycap rendered next to the row goes through the same per-platform resolution every other
    /// keycap in this app does, so a Ctrl/⌘ mismatch can't be introduced here.
    pub fn keystroke_spec(self) -> Option<&'static str> {
        match self {
            MenuAction::Rename => Some("F2"),
            MenuAction::Cut => Some("mod+X"),
            MenuAction::Copy => Some("mod+C"),
            MenuAction::Paste => Some("mod+V"),
            _ => None,
        }
    }

    /// Whether this action mutates or destroys something on disk - the rows the menu tints with
    /// `theme::status::FAIL` so a destructive click is never visually identical to `Copy Path`.
    pub fn is_destructive(self) -> bool {
        matches!(self, MenuAction::Delete)
    }
}

/// One rendered row: an action, plus whether it can actually be run right now. A disabled row is
/// still shown (so the menu's shape doesn't jump between right-clicks) but is not clickable and
/// carries the real reason as its tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    pub action: MenuAction,
    pub enabled: bool,
    /// Why a disabled row is disabled - `None` when it's enabled.
    pub disabled_reason: Option<&'static str>,
}

impl MenuItem {
    fn enabled(action: MenuAction) -> Self {
        MenuItem {
            action,
            enabled: true,
            disabled_reason: None,
        }
    }

    fn gated(action: MenuAction, enabled: bool, reason: &'static str) -> Self {
        MenuItem {
            action,
            enabled,
            disabled_reason: (!enabled).then_some(reason),
        }
    }
}

/// The real rows for `target`, in the order issue #19 §1 lists them.
///
/// `clipboard_has_entry` gates every `Paste` row: with nothing cut or copied there is genuinely
/// nothing to paste, and a row that silently does nothing is worse than a visibly disabled one.
pub fn menu_items(target: &ContextTarget, clipboard_has_entry: bool) -> Vec<MenuItem> {
    const NOTHING_COPIED: &str = "nothing has been cut or copied yet";
    match target {
        ContextTarget::File(_) => vec![
            MenuItem::enabled(MenuAction::Open),
            MenuItem::enabled(MenuAction::Rename),
            MenuItem::enabled(MenuAction::Duplicate),
            MenuItem::enabled(MenuAction::Cut),
            MenuItem::enabled(MenuAction::Copy),
            MenuItem::gated(MenuAction::Paste, clipboard_has_entry, NOTHING_COPIED),
            MenuItem::enabled(MenuAction::CopyPath),
            MenuItem::enabled(MenuAction::CopyRelativePath),
            MenuItem::enabled(MenuAction::Delete),
            MenuItem::enabled(MenuAction::Reveal),
        ],
        ContextTarget::Folder(_) => vec![
            MenuItem::enabled(MenuAction::NewFile),
            MenuItem::enabled(MenuAction::NewFolder),
            MenuItem::enabled(MenuAction::Rename),
            MenuItem::enabled(MenuAction::Cut),
            MenuItem::enabled(MenuAction::Copy),
            MenuItem::gated(MenuAction::Paste, clipboard_has_entry, NOTHING_COPIED),
            MenuItem::enabled(MenuAction::CollapseSubtree),
            MenuItem::enabled(MenuAction::CopyPath),
            MenuItem::enabled(MenuAction::Delete),
            MenuItem::enabled(MenuAction::Reveal),
        ],
        ContextTarget::Empty => vec![
            MenuItem::enabled(MenuAction::NewFile),
            MenuItem::enabled(MenuAction::NewFolder),
            MenuItem::gated(MenuAction::Paste, clipboard_has_entry, NOTHING_COPIED),
            MenuItem::enabled(MenuAction::CollapseAll),
        ],
    }
}

/// The popover's fixed width, in px. Wide enough for the longest label
/// ("Copy Relative Path") plus a `Ctrl+V`-sized keycap without wrapping.
pub const MENU_WIDTH: f32 = 208.0;

/// One menu row's height, in px - matching `theme::band::TREE_ROW`'s own 22px rhythm plus a
/// little breathing room, since these rows carry a keycap the tree rows don't.
pub const MENU_ROW_HEIGHT: f32 = 24.0;

/// The popover's own vertical padding (top + bottom together), in px.
pub const MENU_VERTICAL_PADDING: f32 = 8.0;

/// The smallest gap the menu keeps from a window edge, in px - so a menu pushed back inside the
/// window doesn't sit flush against the frame.
pub const MENU_EDGE_MARGIN: f32 = 4.0;

/// The popover's own 1px border, top and bottom. Part of the painted height because GPUI sizes
/// are border-box only for an *explicit* size; this panel's height comes from its children plus
/// its own padding and border, so a `menu_height` that omitted this was 2px short of what really
/// paints - which is exactly enough for a menu opened at the very bottom edge to overhang.
pub const MENU_BORDER: f32 = 2.0;

/// The popover's real painted height for `rows` rows.
pub fn menu_height(rows: usize) -> f32 {
    MENU_ROW_HEIGHT * rows as f32 + MENU_VERTICAL_PADDING + MENU_BORDER
}

/// Where the popover's top-left corner must go so the whole menu stays inside a
/// `viewport_width` x `viewport_height` window, given the click it was opened from (issue #19 §1:
/// "the menu repositions to stay inside the window near screen edges").
///
/// The rule, per axis, in order:
/// 1. Draw from the click, the way every context menu does.
/// 2. If that overflows the far edge, *flip* to the other side of the click (right-click near the
///    right edge opens leftwards; near the bottom, upwards). A flip is preferred over a slide
///    because it keeps the cursor on the menu's corner, so the pointer doesn't end up hovering a
///    row the user never aimed at.
/// 3. If the flip would overflow the near edge too - a menu taller than the window, or a click in
///    a corner of a very small window - clamp into the window and accept that the cursor is no
///    longer at a corner. Clamping is applied last and against both edges, so the returned origin
///    is never negative and never past the far edge whenever the menu genuinely fits.
///
/// Returns window-space pixels, the same space `gpui::MouseDownEvent::position` is in.
pub fn clamp_menu_origin(
    click_x: f32,
    click_y: f32,
    width: f32,
    height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> (f32, f32) {
    (
        place_axis(click_x, width, viewport_width),
        place_axis(click_y, height, viewport_height),
    )
}

fn place_axis(click: f32, size: f32, viewport: f32) -> f32 {
    let far_limit = viewport - size - MENU_EDGE_MARGIN;
    let mut origin = click;
    if origin > far_limit {
        // Flip to the other side of the click.
        origin = click - size;
    }
    // `far_limit` can be smaller than `MENU_EDGE_MARGIN` when the menu is larger than the
    // window; `max` last so the near edge always wins in that case (showing the menu's top-left,
    // which is where its first rows are, rather than its bottom-right).
    origin.min(far_limit).max(MENU_EDGE_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(items: &[MenuItem]) -> Vec<MenuAction> {
        items.iter().map(|item| item.action).collect()
    }

    /// Issue #19 §1's three lists, exactly.
    #[test]
    fn each_target_offers_the_actions_the_issue_lists() {
        let file = ContextTarget::File(PathBuf::from("/repo/a.rs"));
        assert_eq!(
            actions(&menu_items(&file, true)),
            vec![
                MenuAction::Open,
                MenuAction::Rename,
                MenuAction::Duplicate,
                MenuAction::Cut,
                MenuAction::Copy,
                MenuAction::Paste,
                MenuAction::CopyPath,
                MenuAction::CopyRelativePath,
                MenuAction::Delete,
                MenuAction::Reveal,
            ]
        );

        let folder = ContextTarget::Folder(PathBuf::from("/repo/src"));
        assert_eq!(
            actions(&menu_items(&folder, true)),
            vec![
                MenuAction::NewFile,
                MenuAction::NewFolder,
                MenuAction::Rename,
                MenuAction::Cut,
                MenuAction::Copy,
                MenuAction::Paste,
                MenuAction::CollapseSubtree,
                MenuAction::CopyPath,
                MenuAction::Delete,
                MenuAction::Reveal,
            ]
        );

        assert_eq!(
            actions(&menu_items(&ContextTarget::Empty, true)),
            vec![
                MenuAction::NewFile,
                MenuAction::NewFolder,
                MenuAction::Paste,
                MenuAction::CollapseAll,
            ]
        );
    }

    #[test]
    fn paste_is_the_only_row_an_empty_clipboard_disables() {
        for target in [
            ContextTarget::File(PathBuf::from("/repo/a.rs")),
            ContextTarget::Folder(PathBuf::from("/repo/src")),
            ContextTarget::Empty,
        ] {
            let items = menu_items(&target, false);
            for item in &items {
                assert_eq!(
                    item.enabled,
                    item.action != MenuAction::Paste,
                    "{:?} on {target:?}",
                    item.action
                );
            }
            assert!(items
                .iter()
                .find(|item| item.action == MenuAction::Paste)
                .expect("every target has a Paste row")
                .disabled_reason
                .is_some());
        }
    }

    #[test]
    fn a_targets_destination_directory_is_where_a_new_entry_would_land() {
        let root = Path::new("/repo");
        assert_eq!(
            ContextTarget::Folder(PathBuf::from("/repo/src")).destination_dir(root),
            Path::new("/repo/src")
        );
        assert_eq!(
            ContextTarget::File(PathBuf::from("/repo/src/main.rs")).destination_dir(root),
            Path::new("/repo/src"),
            "a new file next to a file lands in that file's own folder"
        );
        assert_eq!(ContextTarget::Empty.destination_dir(root), root);
    }

    #[test]
    fn a_menu_that_fits_opens_exactly_at_the_click() {
        let height = menu_height(10);
        assert_eq!(
            clamp_menu_origin(100.0, 50.0, MENU_WIDTH, height, 1200.0, 800.0),
            (100.0, 50.0)
        );
    }

    #[test]
    fn a_click_near_the_right_or_bottom_edge_flips_the_menu_back_inside() {
        let height = menu_height(10);
        let (x, y) = clamp_menu_origin(1190.0, 790.0, MENU_WIDTH, height, 1200.0, 800.0);
        assert_eq!(x, 1190.0 - MENU_WIDTH, "flips leftwards off the click");
        assert_eq!(y, 790.0 - height, "flips upwards off the click");
        assert!(x >= MENU_EDGE_MARGIN && x + MENU_WIDTH <= 1200.0);
        assert!(y >= MENU_EDGE_MARGIN && y + height <= 800.0);
    }

    /// A menu taller than the window can't fit either way round; it must still start inside the
    /// window (showing its first rows) rather than at a negative offset.
    #[test]
    fn a_menu_larger_than_the_window_clamps_to_the_near_edge() {
        let height = menu_height(10);
        let (x, y) = clamp_menu_origin(20.0, 20.0, MENU_WIDTH, height, 100.0, 60.0);
        assert_eq!((x, y), (MENU_EDGE_MARGIN, MENU_EDGE_MARGIN));
    }

    /// A click in the very top-left corner would flip a menu to a negative origin; the near-edge
    /// clamp is what stops that.
    #[test]
    fn a_flip_never_produces_a_negative_origin() {
        let height = menu_height(4);
        let (x, y) = clamp_menu_origin(2.0, 2.0, MENU_WIDTH, height, 240.0, 60.0);
        assert!(x >= MENU_EDGE_MARGIN, "got {x}");
        assert!(y >= MENU_EDGE_MARGIN, "got {y}");
    }

    /// Asserted absolutely, not just as a growth rate. A rate-only assertion is blind to a
    /// constant term going missing, which is exactly how the panel's own 2px border was left out
    /// of an earlier version of `menu_height` - making every edge-flip 2px optimistic.
    #[test]
    fn menu_height_is_the_panels_whole_painted_height() {
        assert_eq!(
            menu_height(4),
            4.0 * MENU_ROW_HEIGHT + MENU_VERTICAL_PADDING + MENU_BORDER,
            "every term `crate::sidebar::render::AdeApp::render_tree_context_menu` actually \
             paints - rows, the panel's own py, and its 1px border top and bottom"
        );
        assert_eq!(
            menu_height(5) - menu_height(4),
            MENU_ROW_HEIGHT,
            "and it must still track the real row count"
        );
    }

    #[test]
    fn only_delete_is_marked_destructive() {
        for target in [
            ContextTarget::File(PathBuf::from("/repo/a.rs")),
            ContextTarget::Folder(PathBuf::from("/repo/src")),
            ContextTarget::Empty,
        ] {
            for item in menu_items(&target, true) {
                assert_eq!(
                    item.action.is_destructive(),
                    item.action == MenuAction::Delete,
                    "{:?}",
                    item.action
                );
            }
        }
    }
}
