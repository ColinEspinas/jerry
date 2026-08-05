//! The one real "only one menu is open at a time, and it closes when interaction moves away"
//! mechanism, shared by every floating dropdown/context-menu surface in the app (GitHub issue
//! #176).
//!
//! Before this module each of the six menu surfaces owned an independent `Option`/`bool` with no
//! knowledge of its siblings, and only two hand-added pairwise rules existed (graph row ↔ graph
//! push, and title bar → `+`). Everything else could genuinely stack: opening the tab strip's `+`
//! menu and then right-clicking a file tree row left *both* popovers painted at once, because the
//! `+` menu's scrim only listens for a *left* click and the file tree's own right-click handler
//! never looked at `plus_menu_open`. That is exactly the "2 context menus open at the same time"
//! the issue reports.
//!
//! [`MenuSurface`] is the fix's shape: a payload-free name for each surface, an `ALL` array, and
//! two exhaustive `match`es ([`AdeApp::menu_surface_is_open`]/[`AdeApp::close_menu_surface`]).
//! Adding a seventh popover without teaching it the invariant is a compile error, not a silent
//! drift - the same reason `crate::root::widgets::menu_popover_chrome` is a shared *function*
//! rather than a shared set of style constants.
//!
//! The state each surface owns stays where it is (it carries per-surface payload - which title
//! menu, which tree row, which graph commit - that a common field could not hold), so callers
//! that open a payload-carrying menu run [`AdeApp::close_menu_surfaces_except`] and then assign
//! their own field, rather than this module trying to own all six shapes.

use super::*;

/// Every real floating menu/dropdown surface in the app - the six built on
/// [`crate::root::widgets::menu_popover_chrome`].
///
/// Deliberately *not* including the command palette, the "New file" prompt, or the Settings
/// surface: those are focus-owning modal overlays with their own capture/restore contract
/// (`crate::root::mod`'s [`OverlayFocus`]), not click-away menus, and they already close each
/// other explicitly. They do, however, close *these* - see
/// [`crate::root::focus::AdeApp::open_palette`]/`open_settings`.
///
/// Payload-free on purpose: which title menu is open, which tree row a context menu targets, and
/// which graph commit a row menu names all stay in the surfaces' own fields. This enum only
/// answers "is it open" and "close it", which is all the shared invariant needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuSurface {
    /// The tab strip's (and rail repo header's) `+` menu - [`AdeApp::plus_menu_open`].
    Plus,
    /// The Windows/Linux title bar's File/Edit/View/Agent/Help dropdowns -
    /// [`AdeApp::title_menu_open`].
    Title,
    /// The file tree's right-click context menu - [`AdeApp::tree_context_menu`].
    TreeContext,
    /// The commit composer's `▾` split-button menu - [`AdeApp::commit_menu_open`].
    Commit,
    /// The git graph toolbar's `Push ▾` menu - `graph_state.push_menu_open`.
    GraphPush,
    /// A git graph row's `⋯`/right-click menu - `graph_state.row_menu_open`.
    GraphRow,
}

impl MenuSurface {
    /// Every variant. The `match`es in [`AdeApp::menu_surface_is_open`] and
    /// [`AdeApp::close_menu_surface`] are exhaustive, so a new variant added here cannot compile
    /// until it is really wired to real state - that pairing is what stops a seventh menu from
    /// quietly opting out of the invariant.
    pub(crate) const ALL: [MenuSurface; 6] = [
        MenuSurface::Plus,
        MenuSurface::Title,
        MenuSurface::TreeContext,
        MenuSurface::Commit,
        MenuSurface::GraphPush,
        MenuSurface::GraphRow,
    ];
}

impl AdeApp {
    /// Whether `surface`'s popover is currently open, read from that surface's own real state
    /// field (never a duplicate "which menu is open" mirror that could disagree with it).
    pub(crate) fn menu_surface_is_open(&self, surface: MenuSurface) -> bool {
        match surface {
            MenuSurface::Plus => self.plus_menu_open,
            MenuSurface::Title => self.title_menu_open.is_some(),
            MenuSurface::TreeContext => self.tree_context_menu.is_some(),
            MenuSurface::Commit => self.commit_menu_open,
            MenuSurface::GraphPush => self.graph_state.push_menu_open,
            MenuSurface::GraphRow => self.graph_state.row_menu_open.is_some(),
        }
    }

    /// Closes `surface`, leaving every other one alone. Does **not** `cx.notify()` - the callers
    /// below batch a single notify for the whole sweep.
    ///
    /// The tree context menu goes through [`Self::close_tree_context_menu`]'s own plain field
    /// clear rather than that method (which takes a `Context` and notifies); the extra work that
    /// method does is exactly the notify this one deliberately defers.
    pub(crate) fn close_menu_surface(&mut self, surface: MenuSurface) {
        match surface {
            MenuSurface::Plus => {
                self.plus_menu_open = false;
                self.plus_menu_repo_anchor = None;
            }
            MenuSurface::Title => self.title_menu_open = None,
            MenuSurface::TreeContext => self.tree_context_menu = None,
            MenuSurface::Commit => self.commit_menu_open = false,
            MenuSurface::GraphPush => self.graph_state.push_menu_open = false,
            MenuSurface::GraphRow => self.graph_state.row_menu_open = None,
        }
    }

    /// Closes every open menu surface except `keep` (pass `None` to close all of them). Returns
    /// whether anything was actually open and got closed, so callers can skip a pointless
    /// `cx.notify()`.
    ///
    /// This is the whole "opening a second menu closes the first" rule. Every real path that
    /// *opens* one of the six calls it with its own surface as `keep` immediately before setting
    /// its own state - so the invariant holds no matter which of the (many) entry points was used:
    /// a click on the tab strip `+`, a click on a rail repo header's `+`, a title bar label, a
    /// right-click in the file tree, the commit composer's `▾`, the graph's `Push ▾`, a graph
    /// row's `⋯`, or a right-click on a graph row.
    #[must_use]
    pub(crate) fn close_menu_surfaces_except(&mut self, keep: Option<MenuSurface>) -> bool {
        let mut closed_any = false;
        for surface in MenuSurface::ALL {
            if Some(surface) == keep || !self.menu_surface_is_open(surface) {
                continue;
            }
            self.close_menu_surface(surface);
            closed_any = true;
        }
        closed_any
    }

    /// Closes every open menu surface and notifies if that changed anything. The "interaction
    /// moved somewhere a menu has no business staying open over" entry point: the window itself
    /// losing OS focus ([`Self::_window_activation_subscription`]), and opening one of the
    /// focus-owning overlays (the palette, Settings).
    pub(crate) fn close_all_menu_surfaces(&mut self, cx: &mut Context<Self>) -> bool {
        let closed_any = self.close_menu_surfaces_except(None);
        if closed_any {
            cx.notify();
        }
        closed_any
    }
}

/// Real coverage for the shared invariant itself. The per-surface interaction tests (a real
/// simulated click on one menu's real trigger while another is really open) live beside each
/// surface's own tests - see `crate::work_surface::render`'s `plus_menu_tests`,
/// `crate::sidebar::render`'s `commit_composer_tests`, and `crate::sidebar::tree_ops`'s
/// `context_menu_tests`.
#[cfg(test)]
mod menu_surface_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests::open_test_app;
    use gpui::TestAppContext;

    /// Opens every one of the six at once by writing their real state fields directly - the state
    /// this test then proves cannot survive a single `close_menu_surfaces_except`.
    fn open_every_menu(app: &mut AdeApp) {
        app.plus_menu_open = true;
        app.title_menu_open = Some(crate::title_bar::menu::TitleMenu::File);
        app.tree_context_menu = Some(crate::sidebar::tree_ops::TreeContextMenu {
            target: crate::sidebar::context_menu::ContextTarget::Empty,
            origin_x: 4.0,
            origin_y: 4.0,
        });
        app.commit_menu_open = true;
        app.graph_state.push_menu_open = true;
        app.graph_state.row_menu_open = Some(crate::graph_view::state::GraphRowMenu {
            row_index: 0,
            origin_x: px(4.0),
            origin_y: px(4.0),
        });
    }

    #[gpui::test]
    fn closing_all_menu_surfaces_really_closes_every_one_of_the_six(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            open_every_menu(app);
            for surface in MenuSurface::ALL {
                assert!(
                    app.menu_surface_is_open(surface),
                    "premise: {surface:?} must really be open before the sweep - otherwise this \
                     test would pass without closing anything"
                );
            }
            assert!(
                app.close_all_menu_surfaces(cx),
                "the sweep must report that it really closed something"
            );
            for surface in MenuSurface::ALL {
                assert!(
                    !app.menu_surface_is_open(surface),
                    "{surface:?} must really be closed by close_all_menu_surfaces - a surface \
                     missing from the sweep is exactly the drift MenuSurface::ALL exists to stop"
                );
            }
            assert!(
                !app.close_all_menu_surfaces(cx),
                "a second sweep with nothing open must report no change, so callers can skip a \
                 pointless notify"
            );
        });
    }

    /// The exact "2 context menus open at the same time" shape from GitHub issue #176, at the
    /// level of the shared primitive: whichever surface is being opened is the only survivor.
    #[gpui::test]
    fn opening_any_one_surface_closes_all_five_others(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        for keep in MenuSurface::ALL {
            app.update(cx, |app, _cx| {
                open_every_menu(app);
                assert!(
                    app.close_menu_surfaces_except(Some(keep)),
                    "with all six open, keeping {keep:?} must really close the other five"
                );
                assert!(
                    app.menu_surface_is_open(keep),
                    "{keep:?} is the surface being opened - the sweep must leave it alone"
                );
                for other in MenuSurface::ALL {
                    if other == keep {
                        continue;
                    }
                    assert!(
                        !app.menu_surface_is_open(other),
                        "{other:?} must not still be open next to {keep:?} - two menus painted \
                         at once is the reported bug"
                    );
                }
                let _ = app.close_menu_surfaces_except(None);
            });
        }
    }

    /// The issue's headline requirement - "when a dropdown loses focus it should be closed" - at
    /// its coarsest real granularity: the whole window losing OS focus. Driven through GPUI's own
    /// `VisualTestContext::deactivate_window`, which really runs the platform active-status
    /// callback and so really fires [`AdeApp::_window_activation_subscription`]; nothing here
    /// calls the close path directly.
    #[gpui::test]
    fn the_window_losing_focus_really_closes_every_open_menu(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        // `deactivate_window` is a no-op unless this window really is the platform's active one,
        // and `TestWindow` does not start active.
        cx.update(|window, _cx| window.activate_window());
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            open_every_menu(app);
            cx.notify();
        });
        cx.run_until_parked();
        assert!(
            app.read_with(cx, |app, _| app.plus_menu_open && app.commit_menu_open),
            "premise: menus must really be open before the window is deactivated"
        );

        cx.deactivate_window();
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            for surface in MenuSurface::ALL {
                assert!(
                    !app.menu_surface_is_open(surface),
                    "{surface:?} must be closed once the window itself loses focus - a menu is \
                     anchored to a control in *this* window, and coming back from another app to \
                     a still-open popover positioned off possibly-moved bounds is the bug"
                );
            }
        });
    }

    /// The `+` menu's repo anchor is part of "which `+` menu is open", not a separate fact: a
    /// stale anchor left behind by a rail repo header's `+` would position the *next* tab-strip
    /// `+` menu off that repo row instead of off the `+` button.
    #[gpui::test]
    fn closing_the_plus_menu_also_clears_its_repo_anchor(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, cx| {
            let repo_id = crate::rail::repo::RepoId(7);
            app.plus_menu_open = true;
            app.plus_menu_repo_anchor = Some(repo_id);
            assert!(app.close_all_menu_surfaces(cx));
            assert_eq!(
                app.plus_menu_repo_anchor, None,
                "the anchor must be cleared with the menu it belongs to, or the next tab-strip \
                 + menu would paint off a rail repo header's bounds"
            );
        });
    }
}
