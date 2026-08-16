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
//! Adding another popover without teaching it the invariant is a compile error, not a silent
//! drift - the same reason `crate::root::widgets::menu_popover_chrome` is a shared *function*
//! rather than a shared set of style constants.
//!
//! The state each surface owns stays where it is (it carries per-surface payload - which title
//! menu, which tree row, which graph commit - that a common field could not hold), so callers
//! that open a payload-carrying menu run [`AdeApp::close_menu_surfaces_except`] and then assign
//! their own field, rather than this module trying to own all six shapes.

use super::*;

/// Every real floating menu/dropdown surface in the app - the thirteen built on
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
    /// The tab strip's `+` menu - [`AdeApp::plus_menu_open`]. The rail repo header's own `+` used
    /// to open this same popover too; it is inert now (see
    /// `crate::rail::render::AdeApp::render_repo_group_new_button`), so the tab strip is the
    /// only opener left.
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
    /// A git graph Branches-panel branch row's right-click menu (GitHub issue #241) -
    /// `graph_state.branch_menu_open`. A separate surface from [`Self::GraphRow`] rather than a
    /// second shape of the same field: the two target genuinely different things (a commit vs a
    /// branch), can be open over completely different parts of the window, and must close each
    /// other exactly the way any other two menus do.
    GraphBranch,
    /// Settings › General's "Shell" field suggestion dropdown (GitHub issue #213's follow-up) -
    /// [`AdeApp::shell_suggestions_open`]. The first surface that lives on the Settings page
    /// rather than the workspace; it is a click-away dropdown built on
    /// [`crate::root::widgets::menu_popover_chrome`] like all the others, so it belongs to the
    /// same one-at-a-time invariant rather than owning a second dismissal rule of its own.
    ShellSuggestions,
    /// The rail's worktree/agent row right-click menu (GitHub issue #290) -
    /// [`AdeApp::rail_row_menu`]. The first menu in the rail at all; it joins the invariant here
    /// rather than owning a dismissal rule of its own, exactly like every surface above it.
    RailRow,
    /// The rail's `⋯` overflow menu (GitHub issue #290) - [`AdeApp::rail_overflow_menu`]. A
    /// separate surface from [`Self::RailRow`] for the same reason [`Self::GraphBranch`] is
    /// separate from [`Self::GraphRow`]: the two are anchored differently (a button's rect vs the
    /// pointer), open from different things, and must close each other exactly the way any other
    /// two menus do.
    RailOverflow,
    /// Settings › Notifications' per-event "choose a sound" dropdown (GitHub issue #226) -
    /// [`AdeApp::sound_picker_open`]. Same shape as [`Self::ShellSuggestions`] (a Settings-page
    /// click-away dropdown), keyed by which of the three sound events it's open for rather than a
    /// plain bool, since there are three independent triggers on one page instead of one.
    SoundPicker,
    /// The status bar's Resources popover (GitHub issue #293) - [`AdeApp::resources_popover_open`].
    /// The first surface anchored to the *bottom* chrome rather than to something inside the
    /// workspace body; it is a click-away popover built on
    /// [`crate::root::widgets::menu_popover_chrome`] like all the others, so it belongs to the
    /// same one-at-a-time invariant rather than owning a second dismissal rule of its own.
    Resources,
    /// The agent pane strip's rate-limit budget popover (GitHub issue #294) -
    /// [`AdeApp::budget_popover_open`]. Anchored to a readout *inside the workspace body* rather
    /// than to window chrome, but it is the same click-away popover built on
    /// [`crate::root::widgets::menu_popover_chrome`], so it belongs to the same one-at-a-time
    /// invariant.
    Budget,
}

impl MenuSurface {
    /// Every variant. The `match`es in [`AdeApp::menu_surface_is_open`] and
    /// [`AdeApp::close_menu_surface`] are exhaustive, so a new variant added here cannot compile
    /// until it is really wired to real state - that pairing is what stops a new menu from
    /// quietly opting out of the invariant.
    pub(crate) const ALL: [MenuSurface; 13] = [
        MenuSurface::Plus,
        MenuSurface::Title,
        MenuSurface::TreeContext,
        MenuSurface::Commit,
        MenuSurface::GraphPush,
        MenuSurface::GraphRow,
        MenuSurface::GraphBranch,
        MenuSurface::ShellSuggestions,
        MenuSurface::SoundPicker,
        MenuSurface::RailRow,
        MenuSurface::RailOverflow,
        MenuSurface::Resources,
        MenuSurface::Budget,
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
            MenuSurface::GraphBranch => self.graph_state.branch_menu_open.is_some(),
            MenuSurface::ShellSuggestions => self.shell_suggestions_open,
            MenuSurface::SoundPicker => self.sound_picker_open.is_some(),
            MenuSurface::RailRow => self.rail_row_menu.is_some(),
            MenuSurface::RailOverflow => self.rail_overflow_menu.is_some(),
            MenuSurface::Resources => self.resources_popover_open,
            MenuSurface::Budget => self.budget_popover_open,
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
            MenuSurface::Plus => self.plus_menu_open = false,
            MenuSurface::Title => self.title_menu_open = None,
            MenuSurface::TreeContext => self.tree_context_menu = None,
            MenuSurface::Commit => self.commit_menu_open = false,
            MenuSurface::GraphPush => self.graph_state.push_menu_open = false,
            MenuSurface::GraphRow => self.graph_state.row_menu_open = None,
            // The two-click Delete confirmation belongs to *this* open menu instance and nothing
            // else - a menu closed by any means at all (a click away, another menu opening, the
            // window losing focus) must never leave a branch armed for a one-click delete the
            // next time it opens. `open_graph_branch_menu_at` disarms on open too, for the paths
            // that never route through here.
            MenuSurface::GraphBranch => {
                self.graph_state.branch_menu_open = None;
                self.graph_state.delete_branch_confirm_armed = None;
            }
            MenuSurface::ShellSuggestions => self.shell_suggestions_open = false,
            MenuSurface::SoundPicker => self.sound_picker_open = None,
            // The two-click `Remove worktree…` confirmation belongs to *this* open menu instance
            // and nothing else - a menu closed by any means at all (a click away, another menu
            // opening, the window losing focus) must never leave a worktree armed for a one-click
            // removal the next time it opens. `open_rail_row_menu` disarms on open too, for the
            // paths that never route through here.
            MenuSurface::RailRow => {
                self.rail_row_menu = None;
                self.remove_worktree_confirm_armed = None;
            }
            MenuSurface::RailOverflow => self.rail_overflow_menu = None,
            MenuSurface::Resources => self.resources_popover_open = false,
            MenuSurface::Budget => self.budget_popover_open = false,
        }
    }

    /// Closes every open menu surface except `keep` (pass `None` to close all of them). Returns
    /// whether anything was actually open and got closed, so callers can skip a pointless
    /// `cx.notify()`.
    ///
    /// This is the whole "opening a second menu closes the first" rule. Every real path that
    /// *opens* one of them calls it with its own surface as `keep` immediately before setting
    /// its own state - so the invariant holds no matter which of the (many) entry points was used:
    /// a click on the tab strip `+`, a title bar label, a right-click in the file tree, the commit
    /// composer's `▾`, the graph's `Push ▾`, a graph row's `⋯`, a right-click on a graph row, or a
    /// right-click on a Branches-panel branch row.
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
    use crate::test_support::open_test_app;
    use gpui::TestAppContext;

    /// Opens every one of them at once by writing their real state fields directly - the state
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
        app.graph_state.branch_menu_open = Some(crate::graph_view::state::GraphBranchMenu {
            branch: "main".to_string(),
            origin_x: px(4.0),
            origin_y: px(4.0),
        });
        app.shell_suggestions_open = true;
        app.sound_picker_open = Some(crate::sound::SoundEventKind::AppStart);
        app.rail_row_menu = Some(crate::rail::menu::RailRowMenu {
            target: crate::rail::menu::RailMenuTarget::Worktree(std::path::PathBuf::from("/wt")),
            origin_x: 4.0,
            origin_y: 4.0,
        });
        app.rail_overflow_menu = Some(crate::rail::menu::RailOverflowMenu {
            origin_x: 4.0,
            origin_y: 4.0,
        });
        app.resources_popover_open = true;
        app.budget_popover_open = true;
    }

    #[gpui::test]
    fn closing_all_menu_surfaces_really_closes_every_one_of_them(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
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
    fn opening_any_one_surface_closes_all_the_others(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_root();
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());

        for keep in MenuSurface::ALL {
            app.update(cx, |app, _cx| {
                open_every_menu(app);
                assert!(
                    app.close_menu_surfaces_except(Some(keep)),
                    "with every surface open, keeping {keep:?} must really close all the others"
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
        let repo = crate::test_support::temp_root();
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
}
