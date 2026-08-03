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
//! `crate::work_surface::render::AdeApp::render_plus_menu` already established in this app - with
//! two deliberate differences that scrim does not (yet) have, both forced by a real reported bug:
//! this one `.occlude()`s, so the rows behind it stop taking clicks and painting hover states,
//! and it starts below the title bar rather than at the window top, so occluding it doesn't
//! swallow the window's own caption buttons. See
//! `crate::sidebar::render::AdeApp::render_tree_context_menu`'s own docs.

use std::path::{Path, PathBuf};

/// What a right-click landed on. `Empty` is a real target, not a fallback: the area below the
/// last row offers its own menu (New File / New Folder / Paste / Collapse All), scoped to the
/// worktree root. `Multiple` (GitHub issue #145) is a real, distinct target too, not `File`/
/// `Folder` with an extra field bolted on: a multi-selection has no single "new file goes here"
/// destination and no single name to rename, so it deliberately offers a smaller, honest menu
/// (see [`menu_groups`]) rather than a `File`/`Folder` row set that would silently only ever
/// apply to one of the selected paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTarget {
    File(PathBuf),
    Folder(PathBuf),
    Multiple(Vec<PathBuf>),
    Empty,
}

impl ContextTarget {
    /// The path this target's actions operate *on* - `None` for the empty area and, honestly,
    /// for `Multiple` too: there is no single path a multi-selection's actions operate on, and a
    /// caller that needs the *whole* selection should read [`AdeApp::tree_selected_paths`]
    /// (`crate::sidebar::tree_ops`) instead of asking this method to pick one arbitrarily.
    pub fn path(&self) -> Option<&Path> {
        match self {
            ContextTarget::File(path) | ContextTarget::Folder(path) => Some(path),
            ContextTarget::Multiple(_) | ContextTarget::Empty => None,
        }
    }

    /// The directory a "New File"/"New Folder"/"Paste" from this target should land in: the
    /// folder itself, a file's parent, or the tree root for the empty area. `Multiple` never
    /// offers those rows (see [`menu_groups`]), so this is never actually read for it - the root
    /// is returned anyway rather than panicking, matching every other "not really applicable"
    /// case here.
    pub fn destination_dir<'a>(&'a self, root: &'a Path) -> &'a Path {
        match self {
            ContextTarget::Folder(path) => path,
            ContextTarget::File(path) => path.parent().unwrap_or(root),
            ContextTarget::Multiple(_) | ContextTarget::Empty => root,
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
            // GitHub issue #155: every mutating row with a real registered binding should
            // reference it - `Delete` runs immediately (no confirmation, see
            // `crate::sidebar::tree_ops`'s own module docs) via the same real `"delete"`
            // keybinding this row's own click already does, so leaving its hint blank while
            // Rename/Cut/Copy/Paste all show theirs was the one real inconsistency here.
            MenuAction::Delete => Some("Delete"),
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

/// The real rows for `target`, **grouped** the way issue #19 §1 lists them - one inner `Vec` per
/// logical group, in order. This is the single source of truth for both the flat order
/// ([`menu_items`], which is literally this flattened) and the rendered separators
/// ([`menu_rows`]), so the two can never disagree about where a group ends: there is no second,
/// hand-maintained list of "and the divider goes after row 3".
///
/// `clipboard_has_entry` gates every `Paste` row: with nothing cut or copied there is genuinely
/// nothing to paste, and a row that silently does nothing is worse than a visibly disabled one.
pub fn menu_groups(target: &ContextTarget, clipboard_has_entry: bool) -> Vec<Vec<MenuItem>> {
    const NOTHING_COPIED: &str = "nothing has been cut or copied yet";
    let paste = || MenuItem::gated(MenuAction::Paste, clipboard_has_entry, NOTHING_COPIED);
    match target {
        ContextTarget::File(_) => vec![
            vec![
                MenuItem::enabled(MenuAction::Open),
                MenuItem::enabled(MenuAction::Rename),
                MenuItem::enabled(MenuAction::Duplicate),
            ],
            vec![
                MenuItem::enabled(MenuAction::Cut),
                MenuItem::enabled(MenuAction::Copy),
                paste(),
            ],
            vec![
                MenuItem::enabled(MenuAction::CopyPath),
                MenuItem::enabled(MenuAction::CopyRelativePath),
            ],
            vec![
                MenuItem::enabled(MenuAction::Delete),
                MenuItem::enabled(MenuAction::Reveal),
            ],
        ],
        ContextTarget::Folder(_) => vec![
            vec![
                MenuItem::enabled(MenuAction::NewFile),
                MenuItem::enabled(MenuAction::NewFolder),
                MenuItem::enabled(MenuAction::Rename),
            ],
            vec![
                MenuItem::enabled(MenuAction::Cut),
                MenuItem::enabled(MenuAction::Copy),
                paste(),
            ],
            vec![
                MenuItem::enabled(MenuAction::CollapseSubtree),
                MenuItem::enabled(MenuAction::CopyPath),
            ],
            vec![
                MenuItem::enabled(MenuAction::Delete),
                MenuItem::enabled(MenuAction::Reveal),
            ],
        ],
        // GitHub issue #145: deliberately just Delete. Rename has no single name to edit; New
        // File/New Folder/Paste have no single destination; Cut/Copy/Duplicate/CollapseSubtree/
        // CopyPath all name a *single* real path each - offering them here would either silently
        // act on only one of the selected paths or need a second, parallel bulk implementation of
        // each. Bulk delete alone is real and safe today because `Self::request_tree_delete` was
        // already immediate and per-path, undo-backed (GitHub issue #105) - looping it over every
        // selected path needed no new machinery. A real follow-up, not a permanent gap.
        ContextTarget::Multiple(_) => vec![vec![MenuItem::enabled(MenuAction::Delete)]],
        ContextTarget::Empty => vec![
            vec![
                MenuItem::enabled(MenuAction::NewFile),
                MenuItem::enabled(MenuAction::NewFolder),
            ],
            vec![paste()],
            vec![MenuItem::enabled(MenuAction::CollapseAll)],
        ],
    }
}

/// The real rows for `target`, flattened - issue #19 §1's order exactly. Derived from
/// [`menu_groups`] rather than written out a second time.
pub fn menu_items(target: &ContextTarget, clipboard_has_entry: bool) -> Vec<MenuItem> {
    menu_groups(target, clipboard_has_entry)
        .into_iter()
        .flatten()
        .collect()
}

/// One thing the popover paints, top to bottom: a real action row, or the divider between two of
/// [`menu_groups`]' groups. Issue #19 §1 always described those groups; the shipped menu listed
/// them in the right order and drew them as one undifferentiated run of ten rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuRow {
    Item(MenuItem),
    Separator,
}

/// [`menu_groups`] with a [`MenuRow::Separator`] between each pair of adjacent groups - never a
/// leading or trailing one, and never one around a group that came out empty.
pub fn menu_rows(target: &ContextTarget, clipboard_has_entry: bool) -> Vec<MenuRow> {
    let mut rows = Vec::new();
    for group in menu_groups(target, clipboard_has_entry) {
        if group.is_empty() {
            continue;
        }
        if !rows.is_empty() {
            rows.push(MenuRow::Separator);
        }
        rows.extend(group.into_iter().map(MenuRow::Item));
    }
    rows
}

/// The popover's fixed width, in px. Wide enough for the longest label
/// ("Copy Relative Path") plus a `Ctrl+V`-sized keycap without wrapping.
pub const MENU_WIDTH: f32 = 208.0;

/// One menu row's height, in px - unchanged from what this menu shipped with. Matching
/// `theme::band::TREE_ROW`'s own 22px rhythm plus a little breathing room, since these rows carry
/// a keycap the tree rows don't.
pub const MENU_ROW_HEIGHT: f32 = 24.0;

/// One group separator's whole vertical footprint, in px - a restatement of
/// `crate::root::widgets::MENU_GROUP_DIVIDER_HEIGHT` (its 1px rule plus its 4px margins top and
/// bottom), as a plain `f32` because this module is deliberately GPUI-free and that constant is
/// typed in `gpui::Pixels`. `crate::sidebar::render`'s
/// `the_context_menu_paints_exactly_the_height_it_measures` is the real guard on the restatement:
/// it compares [`menu_height`] against the popover's *actually painted* bounds, so neither this
/// nor any other term here can drift from the element without failing.
///
/// Part of [`menu_height`] because a menu that measured its rows but not its dividers would flip
/// short of a window edge by 9px per group boundary.
pub const MENU_SEPARATOR_HEIGHT: f32 = 9.0;

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

/// The popover's real painted height for exactly the `rows` it is about to paint - action rows
/// and group dividers both, since [`crate::sidebar::render::AdeApp::render_tree_context_menu`]
/// paints both and an edge flip computed from the row count alone would be short by
/// [`MENU_SEPARATOR_HEIGHT`] per group boundary.
pub fn menu_height(rows: &[MenuRow]) -> f32 {
    let painted: f32 = rows
        .iter()
        .map(|row| match row {
            MenuRow::Item(_) => MENU_ROW_HEIGHT,
            MenuRow::Separator => MENU_SEPARATOR_HEIGHT,
        })
        .sum();
    painted + MENU_VERTICAL_PADDING + MENU_BORDER
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

    /// The real group boundaries, asserted by action rather than by index, so renaming or
    /// reordering *within* a group doesn't churn this test but moving a row *across* one does.
    #[test]
    fn each_target_groups_its_actions_the_way_the_issue_describes() {
        let grouped = |target: &ContextTarget| -> Vec<Vec<MenuAction>> {
            menu_groups(target, true)
                .iter()
                .map(|group| actions(group))
                .collect()
        };

        assert_eq!(
            grouped(&ContextTarget::File(PathBuf::from("/repo/a.rs"))),
            vec![
                vec![MenuAction::Open, MenuAction::Rename, MenuAction::Duplicate],
                vec![MenuAction::Cut, MenuAction::Copy, MenuAction::Paste],
                vec![MenuAction::CopyPath, MenuAction::CopyRelativePath],
                vec![MenuAction::Delete, MenuAction::Reveal],
            ]
        );
        assert_eq!(
            grouped(&ContextTarget::Folder(PathBuf::from("/repo/src"))),
            vec![
                vec![
                    MenuAction::NewFile,
                    MenuAction::NewFolder,
                    MenuAction::Rename
                ],
                vec![MenuAction::Cut, MenuAction::Copy, MenuAction::Paste],
                vec![MenuAction::CollapseSubtree, MenuAction::CopyPath],
                vec![MenuAction::Delete, MenuAction::Reveal],
            ]
        );
        assert_eq!(
            grouped(&ContextTarget::Empty),
            vec![
                vec![MenuAction::NewFile, MenuAction::NewFolder],
                vec![MenuAction::Paste],
                vec![MenuAction::CollapseAll],
            ]
        );

        for target in [
            ContextTarget::File(PathBuf::from("/repo/a.rs")),
            ContextTarget::Folder(PathBuf::from("/repo/src")),
            ContextTarget::Empty,
        ] {
            assert!(
                menu_groups(&target, true).iter().all(|g| !g.is_empty()),
                "an empty group would paint a rule with nothing under it: {target:?}"
            );
        }
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

    /// A menu of `count` plain rows and no dividers - the sizing fixture the geometry tests
    /// below use, so they stay about placement rather than about grouping.
    fn plain_rows(count: usize) -> Vec<MenuRow> {
        (0..count)
            .map(|_| MenuRow::Item(MenuItem::enabled(MenuAction::Open)))
            .collect()
    }

    #[test]
    fn a_menu_that_fits_opens_exactly_at_the_click() {
        let height = menu_height(&plain_rows(10));
        assert_eq!(
            clamp_menu_origin(100.0, 50.0, MENU_WIDTH, height, 1200.0, 800.0),
            (100.0, 50.0)
        );
    }

    #[test]
    fn a_click_near_the_right_or_bottom_edge_flips_the_menu_back_inside() {
        let height = menu_height(&plain_rows(10));
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
        let height = menu_height(&plain_rows(10));
        let (x, y) = clamp_menu_origin(20.0, 20.0, MENU_WIDTH, height, 100.0, 60.0);
        assert_eq!((x, y), (MENU_EDGE_MARGIN, MENU_EDGE_MARGIN));
    }

    /// A click in the very top-left corner would flip a menu to a negative origin; the near-edge
    /// clamp is what stops that.
    #[test]
    fn a_flip_never_produces_a_negative_origin() {
        let height = menu_height(&plain_rows(4));
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
            menu_height(&plain_rows(4)),
            4.0 * MENU_ROW_HEIGHT + MENU_VERTICAL_PADDING + MENU_BORDER,
            "every term `crate::sidebar::render::AdeApp::render_tree_context_menu` actually \
             paints - rows, the panel's own py, and its 1px border top and bottom"
        );
        assert_eq!(
            menu_height(&plain_rows(5)) - menu_height(&plain_rows(4)),
            MENU_ROW_HEIGHT,
            "and it must still track the real row count"
        );
        let mut with_one_divider = plain_rows(4);
        with_one_divider.insert(2, MenuRow::Separator);
        assert_eq!(
            menu_height(&with_one_divider) - menu_height(&plain_rows(4)),
            MENU_SEPARATOR_HEIGHT,
            "a divider is painted height too - a menu measured by row count alone would overhang \
             a window edge by 9px per group boundary"
        );
    }

    /// Dividers go *between* groups and nowhere else - never leading, never trailing, never two
    /// in a row - and the rows they separate stay in issue #19 §1's order.
    #[test]
    fn dividers_only_ever_sit_between_two_real_groups() {
        for target in [
            ContextTarget::File(PathBuf::from("/repo/a.rs")),
            ContextTarget::Folder(PathBuf::from("/repo/src")),
            ContextTarget::Empty,
        ] {
            for clipboard in [true, false] {
                let rows = menu_rows(&target, clipboard);
                assert!(!matches!(rows.first(), Some(MenuRow::Separator)));
                assert!(!matches!(rows.last(), Some(MenuRow::Separator)));
                assert!(
                    !rows
                        .windows(2)
                        .any(|pair| pair == [MenuRow::Separator, MenuRow::Separator]),
                    "two dividers in a row means an empty group got a rule of its own"
                );
                let items: Vec<MenuAction> = rows
                    .iter()
                    .filter_map(|row| match row {
                        MenuRow::Item(item) => Some(item.action),
                        MenuRow::Separator => None,
                    })
                    .collect();
                assert_eq!(
                    items,
                    actions(&menu_items(&target, clipboard)),
                    "adding dividers must not reorder or drop a single action: {target:?}"
                );
                assert_eq!(
                    rows.iter()
                        .filter(|row| matches!(row, MenuRow::Separator))
                        .count(),
                    menu_groups(&target, clipboard).len() - 1
                );
            }
        }
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
