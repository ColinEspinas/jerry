//! The one shared, pure-data description of every real menu command the app offers through a
//! `File Edit View Agent Help` menu - what [`MenuCommand::action`] it dispatches, what
//! [`MenuCommand::label`] and [`MenuCommand::keystroke_spec`] it shows, and which
//! [`MenuCommand::rows`]/[`MenuCommand::app_menu_rows`] order it appears in - consumed by both
//! the Windows/Linux in-window popover ([`crate::title_bar::menu`]) and the real macOS
//! `NSApp.mainMenu` (`crate::title_bar::native_menu`) so the two surfaces can never silently
//! drift onto two different command sets (GitHub issue #235).
#![allow(dead_code)]

use crate::root;
use crate::title_bar::menu::TitleMenu;
use gpui::Action;

/// One of the real commands behind the five `File Edit View Agent Help` menus and the macOS
/// application menu (`Hide`/`Hide Others`/`Show All`/`Quit`). Every variant maps to exactly one
/// already-existing or newly-added `gpui::Action` type via [`Self::action`] - never two variants
/// sharing one action type, since that would mean disabling/dispatching one variant also
/// disabled/dispatched the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MenuCommand {
    // File
    OpenFile,
    OpenFolder,
    NewWindow,
    Save,
    Settings,
    CloseWindow,
    // Edit
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    // View
    CommandPalette,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    // Agent
    NewTerminal,
    NewAgentPane,
    NextAgent,
    PreviousAgent,
    ArchiveAgent,
    ReviewAgent,
    KeepAllChanges,
    DiscardWorktree,
    // Help
    Documentation,
    ReportIssue,
    About,
    // App menu (macOS only in practice - a real `NSApp.mainMenu` application-name submenu - but
    // modeled here like every other command rather than carved out as a special case).
    Hide,
    HideOthers,
    ShowAll,
    Quit,
}

/// One row of a rendered menu: either a real, clickable [`MenuCommand`], or a plain visual
/// divider between two groups of them. Kept separate from [`MenuCommand`] itself (rather than a
/// `MenuCommand::Separator` variant) so [`MenuCommand::ALL`] only ever enumerates real,
/// dispatchable commands - a separator is not a command a `TypeId` check or an `action()` call
/// would make sense for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuRow {
    Command(MenuCommand),
    Separator,
}

impl MenuCommand {
    /// Every real command, in declaration order. The three `match`es below
    /// ([`Self::action`], [`Self::label`], [`Self::keystroke_spec`]) are exhaustive over this
    /// same enum, so nothing here can silently end up with only some of the three - the same
    /// guarantee [`crate::root::menus::MenuSurface::ALL`] gives its own two matches.
    pub(crate) const ALL: [MenuCommand; 31] = [
        MenuCommand::OpenFile,
        MenuCommand::OpenFolder,
        MenuCommand::NewWindow,
        MenuCommand::Save,
        MenuCommand::Settings,
        MenuCommand::CloseWindow,
        MenuCommand::Undo,
        MenuCommand::Redo,
        MenuCommand::Cut,
        MenuCommand::Copy,
        MenuCommand::Paste,
        MenuCommand::SelectAll,
        MenuCommand::CommandPalette,
        MenuCommand::ZoomIn,
        MenuCommand::ZoomOut,
        MenuCommand::ResetZoom,
        MenuCommand::NewTerminal,
        MenuCommand::NewAgentPane,
        MenuCommand::NextAgent,
        MenuCommand::PreviousAgent,
        MenuCommand::ArchiveAgent,
        MenuCommand::ReviewAgent,
        MenuCommand::KeepAllChanges,
        MenuCommand::DiscardWorktree,
        MenuCommand::Documentation,
        MenuCommand::ReportIssue,
        MenuCommand::About,
        MenuCommand::Hide,
        MenuCommand::HideOthers,
        MenuCommand::ShowAll,
        MenuCommand::Quit,
    ];

    /// The real `gpui::Action` this command dispatches - a fresh boxed instance every call,
    /// matching every other `Box<dyn Action>` call site in this codebase (there is no reusable
    /// singleton to hand back, since `gpui`'s dispatch APIs take ownership).
    pub(crate) fn action(self) -> Box<dyn Action> {
        match self {
            MenuCommand::OpenFile => Box::new(root::OpenFile),
            MenuCommand::OpenFolder => Box::new(root::OpenFolder),
            MenuCommand::NewWindow => Box::new(root::NewWindow),
            MenuCommand::Save => Box::new(root::EditorSave),
            MenuCommand::Settings => Box::new(root::ToggleSettings),
            MenuCommand::CloseWindow => Box::new(root::CloseWindow),
            MenuCommand::Undo => Box::new(root::TextUndo),
            MenuCommand::Redo => Box::new(root::TextRedo),
            MenuCommand::Cut => Box::new(root::EditorCut),
            MenuCommand::Copy => Box::new(root::EditorCopy),
            MenuCommand::Paste => Box::new(root::EditorPaste),
            MenuCommand::SelectAll => Box::new(root::EditorSelectAll),
            MenuCommand::CommandPalette => Box::new(root::TogglePalette),
            MenuCommand::ZoomIn => Box::new(root::ZoomIn),
            MenuCommand::ZoomOut => Box::new(root::ZoomOut),
            MenuCommand::ResetZoom => Box::new(root::ResetZoom),
            MenuCommand::NewTerminal => Box::new(root::NewTerminal),
            MenuCommand::NewAgentPane => Box::new(root::NewAgentPane),
            MenuCommand::NextAgent => Box::new(root::NextAgent),
            MenuCommand::PreviousAgent => Box::new(root::PreviousAgent),
            MenuCommand::ArchiveAgent => Box::new(root::ArchiveAgent),
            MenuCommand::ReviewAgent => Box::new(root::ReviewAgent),
            MenuCommand::KeepAllChanges => Box::new(root::KeepAllChanges),
            MenuCommand::DiscardWorktree => Box::new(root::DiscardWorktree),
            MenuCommand::Documentation => Box::new(root::OpenDocumentation),
            MenuCommand::ReportIssue => Box::new(root::ReportIssue),
            MenuCommand::About => Box::new(root::About),
            MenuCommand::Hide => Box::new(root::Hide),
            MenuCommand::HideOthers => Box::new(root::HideOthers),
            MenuCommand::ShowAll => Box::new(root::ShowAll),
            MenuCommand::Quit => Box::new(root::Quit),
        }
    }

    /// This command's shared display label - reused verbatim from the labels
    /// `crate::title_bar::menu`'s `*_menu_rows` builders already render, with exactly one
    /// deliberate rename: the File menu's last row was labelled `"Quit"` (its sub-label already
    /// said "close window" - it only ever really called `Window::remove_window`, never a real
    /// app quit) and is now honestly labelled [`MenuCommand::CloseWindow`]'s `"Close Window"`.
    /// The real app-wide quit only exists as [`MenuCommand::Quit`], on the macOS application
    /// menu.
    pub(crate) fn label(self) -> &'static str {
        match self {
            MenuCommand::OpenFile => "Open File\u{2026}",
            MenuCommand::OpenFolder => "Open Folder\u{2026}",
            MenuCommand::NewWindow => "New Window",
            MenuCommand::Save => "Save",
            MenuCommand::Settings => "Settings\u{2026}",
            MenuCommand::CloseWindow => "Close Window",
            MenuCommand::Undo => "Undo",
            MenuCommand::Redo => "Redo",
            MenuCommand::Cut => "Cut",
            MenuCommand::Copy => "Copy",
            MenuCommand::Paste => "Paste",
            MenuCommand::SelectAll => "Select All",
            MenuCommand::CommandPalette => "Command Palette",
            MenuCommand::ZoomIn => "Zoom In",
            MenuCommand::ZoomOut => "Zoom Out",
            MenuCommand::ResetZoom => "Reset Zoom",
            MenuCommand::NewTerminal => "New Terminal",
            MenuCommand::NewAgentPane => "New Agent Pane",
            MenuCommand::NextAgent => "Next Agent",
            MenuCommand::PreviousAgent => "Previous Agent",
            MenuCommand::ArchiveAgent => "Archive Agent",
            MenuCommand::ReviewAgent => "Review Agent",
            MenuCommand::KeepAllChanges => "Keep All Changes",
            MenuCommand::DiscardWorktree => "Discard Worktree",
            MenuCommand::Documentation => "Documentation",
            MenuCommand::ReportIssue => "Report an Issue",
            MenuCommand::About => "About",
            MenuCommand::Hide => "Hide Jerry",
            MenuCommand::HideOthers => "Hide Others",
            MenuCommand::ShowAll => "Show All",
            MenuCommand::Quit => "Quit Jerry",
        }
    }

    /// The `"mod+s"`-style spec `crate::keymap::resolve_combo` already turns into per-platform
    /// keycap glyphs for the popover - reused verbatim from the specs `crate::title_bar::menu`
    /// already passes it today. `None` for every command with no real keystroke behind it today,
    /// including [`MenuCommand::Quit`]: the real `cmd-q` `gpui::KeyBinding` is added directly in
    /// `crate::default_key_bindings` by a later revision, not synthesized from this spec string -
    /// this function only covers combos [`crate::keymap::resolve_combo`] already renders for the
    /// popover.
    pub(crate) fn keystroke_spec(self) -> Option<&'static str> {
        match self {
            MenuCommand::Save => Some("mod+s"),
            MenuCommand::Settings => Some("mod+,"),
            MenuCommand::Undo => Some("mod+z"),
            MenuCommand::Redo => Some("mod+shift+z"),
            MenuCommand::Cut => Some("mod+x"),
            MenuCommand::Copy => Some("mod+c"),
            MenuCommand::Paste => Some("mod+v"),
            MenuCommand::SelectAll => Some("mod+a"),
            MenuCommand::CommandPalette => Some("mod+P"),
            MenuCommand::NewTerminal => Some("ctrl+shift+T"),
            MenuCommand::NewAgentPane => Some("mod+shift+N"),
            MenuCommand::OpenFile
            | MenuCommand::OpenFolder
            | MenuCommand::NewWindow
            | MenuCommand::CloseWindow
            | MenuCommand::ZoomIn
            | MenuCommand::ZoomOut
            | MenuCommand::ResetZoom
            | MenuCommand::NextAgent
            | MenuCommand::PreviousAgent
            | MenuCommand::ArchiveAgent
            | MenuCommand::ReviewAgent
            | MenuCommand::KeepAllChanges
            | MenuCommand::DiscardWorktree
            | MenuCommand::Documentation
            | MenuCommand::ReportIssue
            | MenuCommand::About
            | MenuCommand::Hide
            | MenuCommand::HideOthers
            | MenuCommand::ShowAll
            | MenuCommand::Quit => None,
        }
    }

    /// The canonical row order for one of the five real `File Edit View Agent Help` menus -
    /// reused row-for-row from `crate::title_bar::menu`'s own `*_menu_rows` builders, which this
    /// becomes the shared source of truth for.
    pub(crate) fn rows(menu: TitleMenu) -> &'static [MenuRow] {
        use MenuRow::{Command, Separator};
        match menu {
            TitleMenu::File => &[
                Command(MenuCommand::OpenFile),
                Command(MenuCommand::OpenFolder),
                Command(MenuCommand::NewWindow),
                Separator,
                Command(MenuCommand::Save),
                Command(MenuCommand::Settings),
                Command(MenuCommand::CloseWindow),
            ],
            TitleMenu::Edit => &[
                Command(MenuCommand::Undo),
                Command(MenuCommand::Redo),
                Separator,
                Command(MenuCommand::Cut),
                Command(MenuCommand::Copy),
                Command(MenuCommand::Paste),
                Command(MenuCommand::SelectAll),
            ],
            TitleMenu::View => &[
                Command(MenuCommand::CommandPalette),
                Separator,
                Command(MenuCommand::ZoomIn),
                Command(MenuCommand::ZoomOut),
                Command(MenuCommand::ResetZoom),
            ],
            TitleMenu::Agent => &[
                Command(MenuCommand::NewTerminal),
                Command(MenuCommand::NewAgentPane),
                Separator,
                Command(MenuCommand::NextAgent),
                Command(MenuCommand::PreviousAgent),
                Separator,
                Command(MenuCommand::ReviewAgent),
                Command(MenuCommand::ArchiveAgent),
                Command(MenuCommand::KeepAllChanges),
                Command(MenuCommand::DiscardWorktree),
            ],
            TitleMenu::Help => &[
                Command(MenuCommand::Documentation),
                Command(MenuCommand::ReportIssue),
                Separator,
                Command(MenuCommand::About),
            ],
        }
    }

    /// The macOS application menu's own row order (the submenu under the app's own name, left of
    /// `File`) - `About`, `Settings`, then the real `Hide`/`Hide Others`/`Show All`/`Quit` quartet
    /// only a real `NSApp.mainMenu` can host at all (the Windows/Linux popover has no equivalent
    /// surface for these four, since there is no window-manager-level "hide this app" concept to
    /// wire them to there).
    pub(crate) fn app_menu_rows() -> &'static [MenuRow] {
        use MenuRow::{Command, Separator};
        &[
            Command(MenuCommand::About),
            Separator,
            Command(MenuCommand::Settings),
            Separator,
            Command(MenuCommand::Hide),
            Command(MenuCommand::HideOthers),
            Command(MenuCommand::ShowAll),
            Separator,
            Command(MenuCommand::Quit),
        ]
    }
}

#[cfg(test)]
mod menu_model_tests {
    use super::*;
    use std::any::TypeId;
    use std::collections::HashSet;

    #[test]
    fn every_command_has_a_non_empty_label() {
        for command in MenuCommand::ALL {
            assert!(
                !command.label().is_empty(),
                "{command:?} must have a real, non-empty label"
            );
        }
    }

    #[test]
    fn no_two_commands_share_the_same_action_type() {
        let mut seen: HashSet<TypeId> = HashSet::new();
        for command in MenuCommand::ALL {
            let type_id = command.action().as_any().type_id();
            assert!(
                seen.insert(type_id),
                "{command:?}'s action type is already used by another MenuCommand - two menu \
                 rows must never share one action type"
            );
        }
    }

    #[test]
    fn every_command_appears_in_at_least_one_menu() {
        let mut covered: HashSet<MenuCommand> = HashSet::new();
        for menu in TitleMenu::ALL {
            for row in MenuCommand::rows(menu) {
                if let MenuRow::Command(command) = row {
                    covered.insert(*command);
                }
            }
        }
        for row in MenuCommand::app_menu_rows() {
            if let MenuRow::Command(command) = row {
                covered.insert(*command);
            }
        }

        for command in MenuCommand::ALL {
            assert!(
                covered.contains(&command),
                "{command:?} is declared in MenuCommand::ALL but never appears in any real menu"
            );
        }
    }

    #[test]
    fn file_menu_row_order_matches_the_popover() {
        assert_eq!(
            MenuCommand::rows(TitleMenu::File),
            &[
                MenuRow::Command(MenuCommand::OpenFile),
                MenuRow::Command(MenuCommand::OpenFolder),
                MenuRow::Command(MenuCommand::NewWindow),
                MenuRow::Separator,
                MenuRow::Command(MenuCommand::Save),
                MenuRow::Command(MenuCommand::Settings),
                MenuRow::Command(MenuCommand::CloseWindow),
            ]
        );
    }
}
