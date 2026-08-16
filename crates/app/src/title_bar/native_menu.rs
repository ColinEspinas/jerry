//! GitHub issue #235: the real macOS `NSApp.mainMenu`, built via `gpui::App::set_menus` from the
//! exact same shared model the Windows/Linux in-window popover already reads
//! (`crate::title_bar::menu_model`) - so the two surfaces can never silently offer two different
//! command sets. Only ever compiled on macOS (see this module's `#[cfg(target_os = "macos")]` in
//! `crate::title_bar`'s own module declaration) - Windows/Linux keep the in-window popover as
//! their only menu surface, which is the whole reason `crate::title_bar::menu` exists.
use gpui::{Menu, MenuItem, SystemMenuType};

use crate::title_bar::menu::TitleMenu;
use crate::title_bar::menu_model::{MenuCommand, MenuRow};

/// One real `MenuItem::Action` for `cmd`, built as the literal struct rather than through
/// [`MenuItem::action`]'s own builder: that builder takes `impl Action` *by value* (one concrete
/// action type), while [`MenuCommand::action`] hands back a `Box<dyn Action>` - it has to, since
/// one function returning ~30 different concrete action types has no single concrete type to
/// give back. `gpui::MenuItem::Action` is a public enum variant with public fields precisely so
/// callers holding a `Box<dyn Action>` can still build one directly.
fn menu_item_for(cmd: MenuCommand) -> MenuItem {
    MenuItem::Action {
        name: cmd.label().into(),
        action: cmd.action(),
        os_action: None,
        checked: false,
        disabled: false,
    }
}

/// Turns one [`MenuCommand::rows`]/[`MenuCommand::app_menu_rows`] slice into the real
/// `gpui::MenuItem` list a `gpui::Menu` renders.
fn menu_items(rows: &[MenuRow]) -> Vec<MenuItem> {
    rows.iter()
        .map(|row| match row {
            MenuRow::Separator => MenuItem::separator(),
            MenuRow::Command(cmd) => menu_item_for(*cmd),
        })
        .collect()
}

/// The real menu bar `crate::run` hands to `gpui::App::set_menus`: the macOS application menu
/// (the app's own name, left of `File`), then the five `File Edit View Agent Help` menus in the
/// same order [`TitleMenu::ALL`] gives the Windows/Linux popover.
pub(crate) fn native_menus() -> Vec<Menu> {
    let mut app_menu_items = menu_items(MenuCommand::app_menu_rows());
    // `app_menu_rows()`'s own order is `About, sep, Settings, sep, Hide, HideOthers, ShowAll,
    // sep, Quit` - nine entries, indices 0-8. `ShowAll` is index 6; Services goes right after it,
    // before the separator (currently index 7) that leads into Quit.
    debug_assert_eq!(
        app_menu_items.len(),
        9,
        "MenuCommand::app_menu_rows changed shape - re-check where Services belongs"
    );
    app_menu_items.insert(
        7,
        MenuItem::os_submenu("Services", SystemMenuType::Services),
    );

    let mut menus = vec![Menu::new("Jerry").items(app_menu_items)];
    for menu in TitleMenu::ALL {
        menus.push(Menu::new(menu.label()).items(menu_items(MenuCommand::rows(menu))));
    }
    menus
}

#[cfg(test)]
mod native_menu_tests {
    use super::*;

    #[test]
    fn native_menus_returns_the_application_menu_plus_all_five_real_menus() {
        let menus = native_menus();
        assert_eq!(
            menus.len(),
            6,
            "the application menu plus File/Edit/View/Agent/Help"
        );

        assert_eq!(menus[0].name.as_ref(), "Jerry");
        assert!(
            !menus[0].items.is_empty(),
            "the application menu must have real items"
        );

        let expected_names = ["File", "Edit", "View", "Agent", "Help"];
        for (menu, expected_name) in menus[1..].iter().zip(expected_names) {
            assert_eq!(menu.name.as_ref(), expected_name);
            assert!(
                !menu.items.is_empty(),
                "{expected_name} must have real items"
            );
        }
    }

    #[test]
    fn the_application_menu_has_a_real_quit_item_and_a_services_submenu() {
        let menus = native_menus();
        let app_menu = &menus[0];

        let has_quit = app_menu.items.iter().any(
            |item| matches!(item, MenuItem::Action { name, .. } if name.as_ref() == "Quit Jerry"),
        );
        assert!(has_quit, "the application menu must have a real Quit item");

        let has_services = app_menu
            .items
            .iter()
            .any(|item| matches!(item, MenuItem::SystemMenu(_)));
        assert!(
            has_services,
            "the application menu must have a real Services submenu"
        );
    }

    #[test]
    fn every_menu_has_exactly_as_many_items_as_its_menu_command_rows() {
        let menus = native_menus();
        // The application menu has one extra real item (Services) beyond `app_menu_rows`' own
        // count.
        assert_eq!(menus[0].items.len(), MenuCommand::app_menu_rows().len() + 1);

        for (menu, title_menu) in menus[1..].iter().zip(TitleMenu::ALL) {
            assert_eq!(menu.items.len(), MenuCommand::rows(title_menu).len());
        }
    }
}
