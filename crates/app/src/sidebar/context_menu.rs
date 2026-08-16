//! The file tree's right-click menu, as pure data (GitHub issue #19 §1): which actions a given
//! target offers, and how they group.
//!
//! GPUI-free on purpose, exactly like [`crate::sidebar::file_tree`] beside it - "does a folder
//! offer Collapse Subtree" is a decision that can be tested without a window, and
//! [`crate::sidebar::render`] only *draws* what this module decides.
//!
//! ## What lives here, and what does not any more
//!
//! Everything *generic* about a menu - what a row is, how groups become separated rows, the
//! popover's painted height, and the edge-aware geometry that keeps it on screen - moved to
//! [`crate::menu::model`] when GitHub issue #290 promoted this component into the app's one
//! shared menu - "Both are 'a list of actions', so they are **one menu component** ... rather
//! than two idioms that drift". The
//! file tree draws through that shared popover now; what stays here is only what is genuinely the
//! file tree's own: its targets, its actions, and its row sets.

use std::path::{Path, PathBuf};

use crate::menu::model::{MenuEntry, MenuRow};

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

    /// Whether this action mutates or destroys something on disk - the rows the shared menu
    /// tints with the failure tint so a destructive click is never visually identical to
    /// `Copy Path`.
    pub fn is_destructive(self) -> bool {
        matches!(self, MenuAction::Delete)
    }

    /// This action as a real row of the shared menu - its label, its keycap spec and its
    /// destructive tint, in one place, so no caller can build a row that disagrees with the
    /// action it runs.
    fn entry(self) -> MenuEntry<MenuAction> {
        let mut entry = MenuEntry::new(self, self.label());
        if let Some(spec) = self.keystroke_spec() {
            entry = entry.keys(spec);
        }
        if self.is_destructive() {
            entry = entry.destructive();
        }
        entry
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
pub fn menu_groups(
    target: &ContextTarget,
    clipboard_has_entry: bool,
) -> Vec<Vec<MenuEntry<MenuAction>>> {
    const NOTHING_COPIED: &str = "nothing has been cut or copied yet";
    let paste = || {
        MenuAction::Paste
            .entry()
            .gated(clipboard_has_entry, NOTHING_COPIED)
    };
    let row = MenuAction::entry;
    match target {
        ContextTarget::File(_) => vec![
            vec![
                row(MenuAction::Open),
                row(MenuAction::Rename),
                row(MenuAction::Duplicate),
            ],
            vec![row(MenuAction::Cut), row(MenuAction::Copy), paste()],
            vec![row(MenuAction::CopyPath), row(MenuAction::CopyRelativePath)],
            vec![row(MenuAction::Delete), row(MenuAction::Reveal)],
        ],
        ContextTarget::Folder(_) => vec![
            vec![
                row(MenuAction::NewFile),
                row(MenuAction::NewFolder),
                row(MenuAction::Rename),
            ],
            vec![row(MenuAction::Cut), row(MenuAction::Copy), paste()],
            vec![row(MenuAction::CollapseSubtree), row(MenuAction::CopyPath)],
            vec![row(MenuAction::Delete), row(MenuAction::Reveal)],
        ],
        // GitHub issue #145: deliberately just Delete. Rename has no single name to edit; New
        // File/New Folder/Paste have no single destination; Cut/Copy/Duplicate/CollapseSubtree/
        // CopyPath all name a *single* real path each - offering them here would either silently
        // act on only one of the selected paths or need a second, parallel bulk implementation of
        // each. Bulk delete alone is real and safe today because `Self::request_tree_delete` was
        // already immediate and per-path, undo-backed (GitHub issue #105) - looping it over every
        // selected path needed no new machinery. A real follow-up, not a permanent gap.
        ContextTarget::Multiple(_) => vec![vec![row(MenuAction::Delete)]],
        ContextTarget::Empty => vec![
            vec![row(MenuAction::NewFile), row(MenuAction::NewFolder)],
            vec![paste()],
            vec![row(MenuAction::CollapseAll)],
        ],
    }
}

/// The real rows for `target`, flattened - issue #19 §1's order exactly. Derived from
/// [`menu_groups`] rather than written out a second time.
pub fn menu_items(target: &ContextTarget, clipboard_has_entry: bool) -> Vec<MenuEntry<MenuAction>> {
    menu_groups(target, clipboard_has_entry)
        .into_iter()
        .flatten()
        .collect()
}

/// [`menu_groups`] with the shared menu's own group dividers between them - see
/// [`crate::menu::model::menu_rows`].
pub fn menu_rows(target: &ContextTarget, clipboard_has_entry: bool) -> Vec<MenuRow<MenuAction>> {
    crate::menu::model::menu_rows(menu_groups(target, clipboard_has_entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(items: &[MenuEntry<MenuAction>]) -> Vec<MenuAction> {
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

    /// Every row's label and keycap comes off the action itself, so the rendered row and the
    /// handler it runs cannot describe two different commands.
    #[test]
    fn every_row_carries_its_actions_own_label_and_binding() {
        for item in menu_items(&ContextTarget::File(PathBuf::from("/repo/a.rs")), true) {
            assert_eq!(item.label, item.action.label());
            assert_eq!(item.keystroke_spec, item.action.keystroke_spec());
            assert_eq!(item.destructive, item.action.is_destructive());
        }
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
                        .any(|pair| matches!(pair, [MenuRow::Separator, MenuRow::Separator])),
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
                    item.destructive,
                    item.action == MenuAction::Delete,
                    "{:?}",
                    item.action
                );
            }
        }
    }
}
