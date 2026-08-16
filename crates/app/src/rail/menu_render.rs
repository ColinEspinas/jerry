//! The rail's menus, wired up (GitHub issue #290): opening one off a real right-click, drawing it
//! through the app's one shared popover ([`crate::menu`]), and running what its rows promise.
//!
//! The row sets themselves are [`crate::rail::menu`]'s, and the popover is
//! [`AdeApp::render_menu_overlay`]'s - what is here is only this surface's own glue.
//!
//! ## Both menus render at the root, not in the rail
//!
//! `REVISION-2026-08-14.md` §4, verbatim: "All menus render outside the scrolling list. Inside it
//! they are clipped by the scroller and scroll away from their anchor." The rail's Worktrees list
//! really is a scroller - a real virtualized `gpui::list` (`crate::rail::render::AdeApp::
//! rail_list_state`) painted inside the same `#agent-rail-list` band - so a menu rendered as a
//! child of a row would be clipped by it, would slide away from the pointer it was anchored to on
//! the next wheel event, and would vanish outright whenever its row left the viewport.
//! `STAGE-A-CHANGELOG.md` §4w generalises it: "an overlay anchored in viewport coordinates must
//! live at the root. If it is nested in a panel, every property of that panel - its scroll, its
//! clip, its mount condition - becomes a bug in the overlay." Both of these are therefore
//! children of `crate::root::AdeApp::render`'s own overlay list.

use super::*;
use crate::menu::model as menu_model;
use crate::menu::render::MenuOverlay;
use crate::rail::menu::{
    self as rail_menu, RailMenuAction, RailMenuTarget, RailOverflowMenu, RailRowMenu,
};
use crate::root::menus;
use crate::root::widgets::text_tooltip;
use crate::work_surface::agents::AgentId;

impl AdeApp {
    /// Opens the worktree/agent row menu at a real pointer position, clamped so the whole popover
    /// stays inside the window ([`menu_model::clamp_menu_origin`]).
    ///
    /// Deliberately does **not** change the rail's selection the way the file tree's right-click
    /// does: selecting a rail row switches the whole app's worktree/agent context (and can even
    /// check a different repo out), which is far too much to happen because someone aimed at a
    /// menu. Every row that needs the target selected does that itself, as part of the action the
    /// user actually asked for.
    pub(crate) fn open_rail_row_menu(
        &mut self,
        target: RailMenuTarget,
        click_x: f32,
        click_y: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // GitHub issue #176's invariant: opening any menu closes every other one.
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::RailRow));
        // A fresh menu is never pre-armed: the `Remove worktree…` confirmation belongs to the
        // menu instance it was clicked in, so re-opening must always start from the first click.
        self.remove_worktree_confirm_armed = None;
        let rows = self.rail_menu_rows_for(&target, cx);
        let viewport = window.bounds().size;
        let (origin_x, origin_y) = menu_model::clamp_menu_origin(
            click_x,
            click_y,
            menu_model::MENU_WIDTH,
            menu_model::menu_height(&rows),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        self.rail_row_menu = Some(RailRowMenu {
            target,
            origin_x,
            origin_y,
        });
        cx.notify();
    }

    /// Replaces the open row menu's target in place, keeping its anchor - the `Open in…` row's
    /// second level. Re-clamped against the new row set's own height, so a two-row menu opened
    /// from a six-row one near the window's foot doesn't inherit a flip it no longer needs.
    fn drill_rail_row_menu_into(
        &mut self,
        target: RailMenuTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.rail_row_menu.as_ref() else {
            return;
        };
        let (anchor_x, anchor_y) = (open.origin_x, open.origin_y);
        let rows = self.rail_menu_rows_for(&target, cx);
        let viewport = window.bounds().size;
        let (origin_x, origin_y) = menu_model::clamp_menu_origin(
            anchor_x,
            anchor_y,
            menu_model::MENU_WIDTH,
            menu_model::menu_height(&rows),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        self.rail_row_menu = Some(RailRowMenu {
            target,
            origin_x,
            origin_y,
        });
        cx.notify();
    }

    /// Closes the row menu (and disarms any half-confirmed `Remove worktree…` with it - see
    /// [`crate::root::menus::AdeApp::close_menu_surface`], which is where that pairing lives so
    /// that *every* way of closing this menu disarms, not just this one).
    pub(crate) fn close_rail_row_menu(&mut self, cx: &mut Context<Self>) {
        if self.rail_row_menu.is_some() {
            self.close_menu_surface(menus::MenuSurface::RailRow);
            cx.notify();
        }
    }

    /// Opens the `⋯` overflow under its own button, right edges aligned (§4w).
    pub(crate) fn open_rail_overflow_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.close_menu_surfaces_except(Some(menus::MenuSurface::RailOverflow));
        let button = self.rail_overflow_button_bounds;
        let rows = menu_model::menu_rows(rail_menu::overflow_menu_groups(
            rail_menu::HISTORY_VIEW_AVAILABLE,
        ));
        let viewport = window.bounds().size;
        let (origin_x, origin_y) = menu_model::anchor_menu_below_button(
            menu_model::AnchorRect {
                top: f32::from(button.origin.y),
                right: f32::from(button.origin.x + button.size.width),
                bottom: f32::from(button.origin.y + button.size.height),
            },
            menu_model::MENU_WIDTH,
            menu_model::menu_height(&rows),
            f32::from(viewport.width),
            f32::from(viewport.height),
        );
        self.rail_overflow_menu = Some(RailOverflowMenu { origin_x, origin_y });
        cx.notify();
    }

    /// Closes the `⋯` overflow.
    pub(crate) fn close_rail_overflow_menu(&mut self, cx: &mut Context<Self>) {
        if self.rail_overflow_menu.is_some() {
            self.close_menu_surface(menus::MenuSurface::RailOverflow);
            cx.notify();
        }
    }

    /// Exactly the rows an open menu on `target` paints, read off real live state: the worktree's
    /// real branch, the real number of agents open in it, whether its removal is half-confirmed,
    /// and whether the agent is really running.
    fn rail_menu_rows_for(
        &self,
        target: &RailMenuTarget,
        cx: &gpui::App,
    ) -> Vec<menu_model::MenuRow<RailMenuAction>> {
        let branch = match target {
            RailMenuTarget::Worktree(path) | RailMenuTarget::WorktreeOpenIn(path) => self
                .worktrees
                .iter()
                .find(|item| &item.path == path)
                .and_then(|item| item.branch.clone()),
            RailMenuTarget::Agent(_) => None,
        };
        let agent_count = match target {
            RailMenuTarget::Worktree(path) | RailMenuTarget::WorktreeOpenIn(path) => {
                self.worktree_archivable_agent_ids(path).len()
            }
            RailMenuTarget::Agent(_) => 0,
        };
        let remove_armed = match target {
            RailMenuTarget::Worktree(path) | RailMenuTarget::WorktreeOpenIn(path) => {
                self.remove_worktree_confirm_armed.as_deref() == Some(path.as_path())
            }
            RailMenuTarget::Agent(_) => false,
        };
        let agent_running = match target {
            RailMenuTarget::Agent(id) => self.rail_agent_is_running(*id, cx),
            RailMenuTarget::Worktree(_) | RailMenuTarget::WorktreeOpenIn(_) => false,
        };
        rail_menu::menu_rows(
            target,
            branch.as_deref(),
            agent_count,
            remove_armed,
            agent_running,
        )
    }

    /// Every run `Archive N agents` really ends: the agent sessions open in `worktree_path`, in
    /// the rail's own order.
    ///
    /// Deliberately the same `ProcessKind::is_agent_session` filter [`AdeApp::build_agent_rows`]
    /// uses to decide which agents get a rail row at all, so the count in the label is exactly
    /// the number of rows the user can see under that worktree. A count that also included plain
    /// shells would promise to archive things the worktree row never showed.
    fn worktree_archivable_agent_ids(&self, worktree_path: &std::path::Path) -> Vec<AgentId> {
        self.agents
            .iter_for_cwd(worktree_path.to_path_buf())
            .filter(|agent| agent.kind.is_agent_session())
            .map(|agent| agent.id)
            .collect()
    }

    /// Whether agent `id` is really working right now - the same real
    /// [`crate::rail::status::Status`] its own rail row is painted from, never a second
    /// derivation. `Ask` counts as running: an agent waiting on an answer still has a live
    /// process to stop, which is exactly what `Pause` does.
    fn rail_agent_is_running(&self, id: AgentId, cx: &gpui::App) -> bool {
        self.build_agent_rows(cx)
            .into_iter()
            .find(|row| row.id == id)
            .is_some_and(|row| matches!(row.status, Status::Run | Status::Ask))
    }

    /// Runs one rail menu row. Reads its target off whichever menu is open, exactly like the file
    /// tree's own dispatcher does - a row's action names *what* to do, and the open menu is the
    /// only honest source for *to what*.
    ///
    /// Every row closes the menu, with the two deliberate exceptions the design asks for:
    /// `Open in…` replaces it with its own second level, and the first click of
    /// `Remove worktree…` leaves it open so the confirming click has something to land on.
    pub(crate) fn run_rail_menu_action(
        &mut self,
        action: RailMenuAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.rail_row_menu.as_ref().map(|menu| menu.target.clone());
        let worktree_path = match &target {
            Some(RailMenuTarget::Worktree(path)) | Some(RailMenuTarget::WorktreeOpenIn(path)) => {
                Some(path.clone())
            }
            _ => None,
        };
        let agent_id = match &target {
            Some(RailMenuTarget::Agent(id)) => Some(*id),
            _ => None,
        };

        match action {
            RailMenuAction::OpenIn => {
                if let Some(path) = worktree_path {
                    self.drill_rail_row_menu_into(RailMenuTarget::WorktreeOpenIn(path), window, cx);
                }
                return;
            }
            RailMenuAction::RemoveWorktree => {
                if let Some(path) = worktree_path {
                    // The first click only arms; the menu stays open under the pointer so the
                    // confirming click has a row to land on. The second click really removes the
                    // worktree, and closes the menu with it.
                    if self.request_discard_worktree_path(path, cx) {
                        self.close_rail_row_menu(cx);
                    } else {
                        cx.notify();
                    }
                }
                return;
            }
            _ => {}
        }

        self.close_rail_row_menu(cx);
        self.close_rail_overflow_menu(cx);

        match action {
            RailMenuAction::NewAgentHere => {
                if let Some(path) = worktree_path {
                    // Selects first, because every spawn path in this app deliberately refuses to
                    // spawn anywhere but the genuinely selected worktree
                    // (`AdeApp::current_worktree_path`'s own docs). `new_agent_pane` - not
                    // `new_agent(ProcessKind::Shell, ..)` - because this row says *agent*: it
                    // spawns the first configured agent CLI a real `$PATH` search finds, which is
                    // also the only kind of process the rail gives a row of its own.
                    self.select_worktree_by_path(&path, window, cx);
                    self.new_agent_pane(cx);
                }
            }
            RailMenuAction::ArchiveWorktreeAgents => {
                if let Some(path) = worktree_path {
                    for id in self.worktree_archivable_agent_ids(&path) {
                        self.archive_agent(id, window, cx);
                    }
                }
            }
            RailMenuAction::CopyBranchName => {
                if let Some(branch) = worktree_path.and_then(|path| {
                    self.worktrees
                        .iter()
                        .find(|item| item.path == path)
                        .and_then(|item| item.branch.clone())
                }) {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(branch));
                }
            }
            RailMenuAction::CopyPath => {
                if let Some(path) = worktree_path {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                        path.display().to_string(),
                    ));
                }
            }
            RailMenuAction::OpenInFileManager => {
                if let Some(path) = worktree_path {
                    self.open_path_with_os_handler(&path, cx);
                }
            }
            RailMenuAction::OpenInTerminal => {
                if let Some(path) = worktree_path {
                    self.select_worktree_by_path(&path, window, cx);
                    // Exactly what `Ctrl+Shift+T` (`crate::root::NewTerminal`) itself runs, which
                    // is why this row carries that keycap.
                    self.new_agent(ProcessKind::Shell, window, cx);
                }
            }
            RailMenuAction::OpenAgent => {
                if let Some(id) = agent_id {
                    self.select_agent(id, window, cx);
                }
            }
            RailMenuAction::PauseAgent => {
                if let Some(id) = agent_id {
                    self.interrupt_agent(id, cx);
                }
            }
            RailMenuAction::ResumeAgent => {
                if let Some(id) = agent_id {
                    self.respawn_agent(id, window, cx);
                }
            }
            RailMenuAction::ArchiveRun => {
                if let Some(id) = agent_id {
                    self.archive_agent(id, window, cx);
                }
            }
            // GitHub issue #227: the overflow's own destination row. Switches the sidebar body to
            // the repo → worktree → run index, which is how History is reached at all - §4t moved
            // it out of the strip, so this row is its only entry point besides a worktree's own
            // `↺ N earlier runs` line.
            RailMenuAction::OpenHistory => self.open_history_view(cx),
            RailMenuAction::OpenSettings => self.open_settings(window, cx),
            // Handled above, before the menu was closed.
            RailMenuAction::OpenIn | RailMenuAction::RemoveWorktree => {}
        }
    }

    /// The open row menu (worktree, agent, or the `Open in…` second level).
    pub(crate) fn render_rail_row_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.rail_row_menu.clone() else {
            return gpui::Empty.into_any_element();
        };
        let rows = self.rail_menu_rows_for(&menu.target, cx);
        self.render_menu_overlay(
            MenuOverlay {
                id: "rail-row-menu",
                origin_x: menu.origin_x,
                origin_y: menu.origin_y,
                rows,
                on_pick: |this, action, window, cx| this.run_rail_menu_action(action, window, cx),
                on_dismiss: |this, cx| this.close_rail_row_menu(cx),
            },
            cx,
        )
    }

    /// The open `⋯` overflow.
    pub(crate) fn render_rail_overflow_menu(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(menu) = self.rail_overflow_menu else {
            return gpui::Empty.into_any_element();
        };
        let rows = crate::menu::model::menu_rows(rail_menu::overflow_menu_groups(
            rail_menu::HISTORY_VIEW_AVAILABLE,
        ));
        self.render_menu_overlay(
            MenuOverlay {
                id: "rail-overflow-menu",
                origin_x: menu.origin_x,
                origin_y: menu.origin_y,
                rows,
                on_pick: |this, action, window, cx| this.run_rail_menu_action(action, window, cx),
                on_dismiss: |this, cx| this.close_rail_overflow_menu(cx),
            },
            cx,
        )
    }

    /// The `⋯` More cell (§4t: "a permanent cell in a 5-cell strip is a claim that you switch to
    /// it constantly. If you don't, it belongs in the overflow").
    ///
    /// It is the strip's last cell, and it is a *cell*: GitHub issue #291 moved it out of the
    /// stop-gap rail header it was parked in and into the sidebar strip that was always its home,
    /// where it goes through `crate::rail::strip_render::strip_cell` like every other child - so
    /// it carries the column rule, the 38px width and the glyph-only hover the rest of the strip
    /// does. It carries a `border-left` rather than the view cells' `border-right`, because it
    /// sits on the far side of the strip's flex spacer.
    ///
    /// None of the menu machinery below changed in that move, exactly as this comment promised it
    /// would not: a `gpui::canvas` still captures the button's real window-space rect (the same
    /// capture the commit composer's `▾` menu uses), because §4w anchors this menu to the
    /// button's own rect rather than to the pointer.
    pub(in crate::rail) fn render_rail_overflow_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        crate::rail::strip_render::strip_cell(
            div()
                .id("rail-overflow")
                .debug_selector(|| "rail-overflow".to_string()),
            "rail-overflow",
            theme::border::RAIL_INNER,
            theme::text::FAINTER,
            div()
                .font(font(theme::font::SANS))
                .text_size(self.ui_text_size(11.0))
                .group_hover("rail-overflow", |el| {
                    el.text_color(theme::text::GLYPH_HOVER)
                })
                .child("\u{22ef}"),
        )
        .border_l_1()
        .border_color(theme::border::INNER)
        .tooltip(text_tooltip("More \u{2014} History and Settings"))
        .child({
            let this = cx.entity();
            gpui::canvas(
                move |bounds, _window, cx| {
                    this.update(cx, |this, _cx| {
                        this.rail_overflow_button_bounds = bounds;
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .top(px(0.0))
            .left(px(0.0))
            .right(px(0.0))
            .bottom(px(0.0))
        })
        .on_click(cx.listener(|this, _event: &ClickEvent, window, cx| {
            cx.stop_propagation();
            if this.rail_overflow_menu.is_some() {
                this.close_rail_overflow_menu(cx);
            } else {
                this.open_rail_overflow_menu(window, cx);
            }
        }))
    }
}

/// Real-window, real-git coverage for the rail's menus (GitHub issue #290): a real right-click on
/// a real painted row, the real popover it opens, and the real thing each row does. Nothing here
/// reaches past the render side to call a handler directly - every gesture is a simulated event
/// at a real painted position, which is the only way these assertions can fail for the reason
/// they claim to.
#[cfg(test)]
mod rail_menu_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests;
    use gpui::TestAppContext;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed in {:?}: {}",
            args,
            dir,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        fs::write(dir.path().join("base.txt"), "base\n").expect("write");
        git(dir.path(), &["add", "base.txt"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    fn add_worktree(repo_path: &Path, branch: &str, name: &str) -> PathBuf {
        let container = TempDir::new().expect("tempdir");
        let path = container.path().join(name);
        drop(container);
        git(
            repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        path
    }

    /// The real `worktree-row-<index>-<path>` selector that row is really painted with.
    fn worktree_row_selector(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        worktree_path: &Path,
    ) -> &'static str {
        let index = app
            .update(cx, |app, cx| app.build_repo_groups(cx))
            .iter()
            .find_map(|group| group.rows.iter().position(|row| row.path == worktree_path))
            .expect("the worktree must be a real, rendered rail row");
        Box::leak(format!("worktree-row-{index}-{}", worktree_path.display()).into_boxed_str())
    }

    /// Spawns a real agent session into the selected worktree - the rail only gives an *agent
    /// row* to a real agent session (`ProcessKind::is_agent_session`), never to a plain shell, so
    /// an agent-row test has to create one.
    fn spawn_agent(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
    ) -> crate::work_surface::agents::AgentId {
        let id = app.update_in(cx, |app, window, cx| {
            app.new_agent(ProcessKind::claude(), window, cx);
            app.agents
                .active()
                .expect("New agent must really spawn an agent")
                .id
        });
        cx.run_until_parked();
        id
    }

    /// Right-clicks agent `id`'s real painted row.
    fn right_click_agent_row(
        cx: &mut gpui::VisualTestContext,
        id: crate::work_surface::agents::AgentId,
    ) {
        let selector: &'static str = Box::leak(format!("agent-row-{id}").into_boxed_str());
        let bounds = cx
            .debug_bounds(selector)
            .expect("the agent row must have painted");
        right_click(cx, bounds.center());
    }

    fn right_click(cx: &mut gpui::VisualTestContext, position: gpui::Point<gpui::Pixels>) {
        cx.simulate_event(gpui::MouseDownEvent {
            button: gpui::MouseButton::Right,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        });
        cx.run_until_parked();
    }

    /// Right-clicks the row for `worktree_path` at its real painted centre.
    fn right_click_worktree_row(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        worktree_path: &Path,
    ) {
        let selector = worktree_row_selector(app, cx, worktree_path);
        let bounds = cx
            .debug_bounds(selector)
            .expect("the worktree row must have painted");
        right_click(cx, bounds.center());
    }

    /// Clicks a real painted menu row by its label.
    fn click_menu_row(cx: &mut gpui::VisualTestContext, menu_id: &str, label: &str) {
        let selector: &'static str = Box::leak(format!("{menu_id}-{label}").into_boxed_str());
        let bounds = cx
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("the row {label:?} must be painted in {menu_id}"));
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
    }

    /// Every label the open menu really painted, top to bottom.
    fn painted_row_labels(
        app: &gpui::Entity<AdeApp>,
        cx: &mut gpui::VisualTestContext,
        menu_id: &'static str,
    ) -> Vec<String> {
        let expected: Vec<String> = app.update(cx, |app, cx| {
            let rows = match menu_id {
                "rail-row-menu" => {
                    let target = app
                        .rail_row_menu
                        .as_ref()
                        .expect("an open menu")
                        .target
                        .clone();
                    app.rail_menu_rows_for(&target, cx)
                }
                _ => menu_model::menu_rows(rail_menu::overflow_menu_groups(
                    rail_menu::HISTORY_VIEW_AVAILABLE,
                )),
            };
            rows.into_iter()
                .filter_map(|row| match row {
                    menu_model::MenuRow::Item(entry) => Some(entry.label),
                    menu_model::MenuRow::Separator => None,
                })
                .collect()
        });
        // Read back off what really painted, in painted order - so a row the model lists but the
        // popover never drew fails here rather than passing on the model's word alone.
        let mut painted: Vec<(f32, String)> = expected
            .into_iter()
            .map(|label| {
                let selector: &'static str =
                    Box::leak(format!("{menu_id}-{label}").into_boxed_str());
                let bounds = cx.debug_bounds(selector).unwrap_or_else(|| {
                    panic!("{label:?} is in the row set but never painted in {menu_id}")
                });
                (f32::from(bounds.origin.y), label)
            })
            .collect();
        painted.sort_by(|a, b| a.0.total_cmp(&b.0));
        painted.into_iter().map(|(_, label)| label).collect()
    }

    /// `REVISION-2026-08-14.md` §4's worktree row set, through a real right-click on a real row:
    /// "`New agent here`, `Archive N agents`, `Copy branch name`, `Copy path`, `Open in…`,
    /// `Remove worktree…`" - and the count really comes from the agents open in *that* worktree.
    #[gpui::test]
    fn right_clicking_a_worktree_row_paints_the_row_set_the_revision_lists(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        // A real agent session in the main worktree, so the Archive row has a real count to
        // report (a plain shell gets no rail row, and so is not something this row promises).
        spawn_agent(&app, cx);

        right_click_worktree_row(&app, cx, repo.path());
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.rail_row_menu.as_ref().map(|menu| menu.target.clone()),
                Some(RailMenuTarget::Worktree(repo.path().to_path_buf())),
                "a real right-click on a worktree row must open that worktree's own menu"
            );
        });
        assert_eq!(
            painted_row_labels(&app, cx, "rail-row-menu"),
            vec![
                "New agent here",
                // The startup shell really lives in the main worktree, so its count is real.
                "Archive 1 agent",
                "Copy branch name",
                "Copy path",
                "Open in\u{2026}",
                "Remove worktree\u{2026}",
            ]
        );

        // A worktree with no agent in it reports that, rather than offering to archive nothing.
        cx.simulate_click(
            gpui::Point::new(px(600.0), px(400.0)),
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();
        right_click_worktree_row(&app, cx, &feature);
        assert!(
            painted_row_labels(&app, cx, "rail-row-menu")
                .contains(&"No agents to archive".to_string()),
            "the Archive row must count the agents in the worktree it was opened on"
        );
    }

    /// §7 rule 1 ("Ship the affordance with the behaviour, or ship neither"), taken literally for
    /// the row that spawns work: `New agent here` really spawns a real agent into the worktree it
    /// was opened on - including one that was not the selected worktree when the menu opened.
    #[gpui::test]
    fn new_agent_here_really_spawns_an_agent_into_that_worktree(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.current_worktree_path().as_deref(),
                Some(repo.path()),
                "premise: the right-clicked worktree must not be the selected one, or this test \
                 could pass on a row that ignored its own target"
            );
        });

        right_click_worktree_row(&app, cx, &feature);
        click_menu_row(cx, "rail-row-menu", "New agent here");
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.agents
                    .iter()
                    .any(|agent| agent.cwd == feature && agent.kind.is_agent_session()),
                "the row must spawn a real agent session into the worktree it was opened on"
            );
            assert_eq!(
                app.current_worktree_path().as_deref(),
                Some(feature.as_path()),
                "and select that worktree, since this app only ever spawns into the selected one"
            );
        });
    }

    /// §4/§6's agent row set: `Open`, exactly one of `Pause`/`Resume`, and one `Archive run` -
    /// with no red `Delete run…` beside it (§7 rule 3).
    #[gpui::test]
    fn right_clicking_an_agent_row_paints_open_one_pause_or_resume_and_one_archive_run(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let agent_id = spawn_agent(&app, cx);
        right_click_agent_row(cx, agent_id);

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.rail_row_menu.as_ref().map(|menu| menu.target.clone()),
                Some(RailMenuTarget::Agent(agent_id)),
                "the agent row's own handler must win over the worktree row's"
            );
        });
        let labels = painted_row_labels(&app, cx, "rail-row-menu");
        assert_eq!(labels.len(), 3, "got {labels:?}");
        assert_eq!(labels[0], "Open");
        assert!(
            labels[1] == "Pause" || labels[1] == "Resume",
            "exactly one of Pause/Resume, never both: {labels:?}"
        );
        assert_eq!(labels[2], "Archive run");
        assert!(
            cx.debug_bounds("rail-row-menu-Delete run\u{2026}")
                .is_none(),
            "there is no red Delete run beside Archive (REVISION-2026-08-14.md §7 rule 3)"
        );
    }

    /// `REVISION-2026-08-14.md` §4, verbatim: "All menus render outside the scrolling list. Inside
    /// it they are clipped by the scroller and scroll away from their anchor."
    ///
    /// Scrolls the real rail list under a really-open menu and asserts the menu did not move a
    /// pixel - with the row it was opened from really moving, so the scroll is proven to have had
    /// an effect rather than the test passing on a scroller that never scrolled.
    #[gpui::test]
    fn scrolling_the_rail_does_not_move_or_clip_an_open_menu(cx: &mut TestAppContext) {
        let repo = init_repo();
        for index in 0..8 {
            add_worktree(
                repo.path(),
                &format!("feature-{index}"),
                &format!("wt{index}"),
            );
        }
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        // Small enough that nine worktree rows genuinely overflow the rail's own viewport -
        // without a list that really scrolls, this test would pass against a menu nailed to a
        // scroller that never moved.
        cx.simulate_resize(gpui::size(px(760.0), px(280.0)));
        cx.run_until_parked();

        let row_selector = worktree_row_selector(&app, cx, repo.path());
        right_click_worktree_row(&app, cx, repo.path());
        let menu_before = cx
            .debug_bounds("rail-row-menu")
            .expect("the menu must paint");
        let row_before = cx.debug_bounds(row_selector).expect("the row must paint");

        app.update(cx, |app, cx| {
            // `gpui::ListState`, not `gpui::ScrollHandle`, now owns the Worktrees list's real
            // scroll offset (`crate::rail::render::AdeApp::rail_list_state`) - see that field's
            // own docs. Same negative-`y`-means-scrolled-down convention `ScrollHandle::
            // set_offset` used, via the identical scrollbar-facing setter
            // `crate::root::scrollbar::ScrollableHandle` calls on a drag.
            app.rail_list_state
                .set_offset_from_scrollbar(gpui::point(px(0.0), px(-40.0)));
            cx.notify();
        });
        cx.run_until_parked();

        let row_after = cx
            .debug_bounds(row_selector)
            .expect("the row must still paint");
        assert_ne!(
            f32::from(row_before.origin.y),
            f32::from(row_after.origin.y),
            "premise: the rail list must really have scrolled - otherwise this test proves nothing"
        );
        let menu_after = cx
            .debug_bounds("rail-row-menu")
            .expect("the menu must still paint after the list under it scrolled");
        assert_eq!(
            (
                f32::from(menu_before.origin.x),
                f32::from(menu_before.origin.y)
            ),
            (
                f32::from(menu_after.origin.x),
                f32::from(menu_after.origin.y)
            ),
            "a menu rendered inside the scroller would have moved with it"
        );
        let list = cx
            .debug_bounds("agent-rail-list")
            .expect("the rail list paints");
        assert!(
            f32::from(menu_after.origin.x) + f32::from(menu_after.size.width)
                > f32::from(list.origin.x) + f32::from(list.size.width),
            "the menu is 206 wide against a much narrower rail - a menu that fitted inside the \
             scroller's box would be one the scroller had clipped"
        );
    }

    /// The `⋯` overflow (§4t/§4u/§4w): History and Settings only, hanging off the button's own
    /// rect with right edges aligned.
    #[gpui::test]
    fn the_overflow_button_opens_history_and_settings_under_its_own_right_edge(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let button = cx
            .debug_bounds("rail-overflow")
            .expect("the overflow button must paint");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.rail_overflow_menu.is_some(),
                "clicking the overflow must open its menu"
            );
        });
        assert_eq!(
            painted_row_labels(&app, cx, "rail-overflow-menu"),
            vec!["History", "Settings"],
            "§4u: History and Settings only - the palette has its own shortcut and its own surface"
        );

        let menu = cx
            .debug_bounds("rail-overflow-menu")
            .expect("the overflow menu must paint");
        assert!(
            ((f32::from(menu.origin.x) + f32::from(menu.size.width))
                - (f32::from(button.origin.x) + f32::from(button.size.width)))
            .abs()
                <= 1.0,
            "§4w: the overflow menu hangs off the button's own rect with right edges aligned"
        );
        assert!(
            f32::from(menu.origin.y) >= f32::from(button.origin.y) + f32::from(button.size.height),
            "and under it, never over the control that opened it"
        );
    }

    /// Every row of the overflow does something on day one (§7 rule 1) - proven by really doing
    /// it: Settings opens the real Settings surface, and the menu closes behind it.
    #[gpui::test]
    fn picking_settings_from_the_overflow_really_opens_settings(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let button = cx.debug_bounds("rail-overflow").expect("button");
        cx.simulate_click(button.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        click_menu_row(cx, "rail-overflow-menu", "Settings");

        app.read_with(cx, |app, _| {
            assert!(
                app.settings_open,
                "the Settings row must really open Settings"
            );
            assert!(
                app.rail_overflow_menu.is_none(),
                "and the menu must close behind it"
            );
        });
    }

    /// §6, verbatim: "Ends the run. It stays in History with its transcript, diffstat and notes;
    /// the files it wrote are untouched." Asserted as behaviour: the run's tab is really gone,
    /// and the worktree and the file it wrote are really still there.
    #[gpui::test]
    fn archive_run_ends_the_run_and_leaves_the_worktree_and_its_files_alone(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();
        let written = repo.path().join("agent-wrote-this.txt");
        fs::write(&written, "work\n").expect("write");

        let agent_id = spawn_agent(&app, cx);
        right_click_agent_row(cx, agent_id);
        click_menu_row(cx, "rail-row-menu", "Archive run");

        app.read_with(cx, |app, _| {
            assert!(
                !app.agents.iter().any(|agent| agent.id == agent_id),
                "Archive run must really end the run"
            );
            assert!(app.rail_row_menu.is_none(), "and close the menu");
        });
        assert!(repo.path().exists(), "the worktree itself is untouched");
        assert!(written.exists(), "and so are the files the run wrote");
    }

    /// `Remove worktree…` routes into the existing two-step discard flow: the first click only
    /// arms (and re-labels the row, leaving the menu open under the pointer), the second really
    /// removes the checkout.
    #[gpui::test]
    fn remove_worktree_takes_two_clicks_and_then_really_removes_the_checkout(
        cx: &mut TestAppContext,
    ) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        right_click_worktree_row(&app, cx, &feature);
        click_menu_row(cx, "rail-row-menu", "Remove worktree\u{2026}");

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.remove_worktree_confirm_armed.as_deref(),
                Some(feature.as_path()),
                "the first click must only arm"
            );
            assert!(
                app.rail_row_menu.is_some(),
                "and leave the menu open, so the confirming click has a row to land on"
            );
        });
        assert!(
            feature.exists(),
            "nothing is removed by the first click of a two-click confirmation"
        );

        click_menu_row(cx, "rail-row-menu", "Remove worktree \u{2014} click again");
        cx.run_until_parked();

        assert!(
            !feature.exists(),
            "the confirming click must really remove the checkout"
        );
        app.read_with(cx, |app, _| {
            assert!(app.rail_row_menu.is_none(), "and close the menu");
            assert!(app.remove_worktree_confirm_armed.is_none());
        });
    }

    /// Closing the menu by *any* means disarms a half-confirmed removal - so the next open can
    /// never be one click away from deleting a checkout.
    #[gpui::test]
    fn dismissing_the_menu_disarms_a_half_confirmed_removal(cx: &mut TestAppContext) {
        let repo = init_repo();
        let feature = add_worktree(repo.path(), "feature", "feature");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        right_click_worktree_row(&app, cx, &feature);
        click_menu_row(cx, "rail-row-menu", "Remove worktree\u{2026}");
        app.read_with(cx, |app, _| {
            assert!(
                app.remove_worktree_confirm_armed.is_some(),
                "premise: armed"
            );
        });

        // A real click away, on the menu's own scrim.
        cx.simulate_click(
            gpui::Point::new(px(700.0), px(500.0)),
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.rail_row_menu.is_none(),
                "the click away must close the menu"
            );
            assert!(
                app.remove_worktree_confirm_armed.is_none(),
                "and disarm with it - a menu re-opened later must start from the first click"
            );
        });
        assert!(feature.exists(), "and nothing may have been removed");
    }

    /// `Open in…` really opens its own second level in the same popover, rather than silently
    /// picking one of the two destinations its ellipsis promises.
    #[gpui::test]
    fn open_in_opens_its_second_level_in_the_same_menu(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        right_click_worktree_row(&app, cx, repo.path());
        click_menu_row(cx, "rail-row-menu", "Open in\u{2026}");

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.rail_row_menu.as_ref().map(|menu| menu.target.clone()),
                Some(RailMenuTarget::WorktreeOpenIn(repo.path().to_path_buf())),
                "the row must open the choice it promises, in the same menu"
            );
        });
        assert_eq!(
            painted_row_labels(&app, cx, "rail-row-menu"),
            vec!["File manager", "Terminal"]
        );
    }

    /// A menu opened at the very foot of the window flips above the pointer and stays painted
    /// inside the window - `REVISION-2026-08-14.md` §4's "flip above when they would overflow the
    /// rail's bottom", asserted against the popover's *painted* bounds rather than its arithmetic.
    #[gpui::test]
    fn a_menu_opened_at_the_windows_foot_flips_above_the_pointer(cx: &mut TestAppContext) {
        let repo = init_repo();
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        cx.run_until_parked();

        let viewport = cx.update(|window, _cx| window.bounds().size);
        let click_y = f32::from(viewport.height) - 6.0;
        app.update_in(cx, |app, window, cx| {
            app.open_rail_row_menu(
                RailMenuTarget::Worktree(repo.path().to_path_buf()),
                12.0,
                click_y,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let menu = cx
            .debug_bounds("rail-row-menu")
            .expect("the menu must paint");
        assert!(
            f32::from(menu.origin.y) < click_y,
            "a menu opened at the foot must flip above the pointer, not off the window"
        );
        assert!(
            f32::from(menu.origin.y) + f32::from(menu.size.height)
                <= f32::from(viewport.height) + 0.5,
            "and its whole painted height must stay inside the window"
        );
    }
}
