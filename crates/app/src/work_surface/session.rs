//! Recording and restoring one worktree's whole tab session - "quit Jerry with some set of tabs
//! open across some set of worktrees and repos, relaunch it, and everything you had open comes
//! back".

use std::path::{Path, PathBuf};

use gpui::{Context, Window};

use crate::root::AdeApp;
use crate::work_surface::agents::{AgentKind, ProcessKind};
use crate::work_surface::state::TabRef;
use crate::work_surface::tab_order_state::{worktree_key, SessionTab};

/// How many [`crate::root::AdeApp::session_restore_notices`] entries are kept. A window left open
/// for days, visiting worktree after worktree, must not accumulate an unbounded `Vec` of strings
/// nothing has read yet; the newest are the ones a user could still act on, so the oldest are
/// dropped first.
pub(crate) const MAX_SESSION_RESTORE_NOTICES: usize = 50;

impl AdeApp {
    /// Records the currently selected worktree's real, live tab strip as its persisted session -
    /// the write side of this module. Called from every real tab mutation (a file tab opened or
    /// closed, an agent or shell spawned or closed, a drag-reorder) and, as a safety net, from
    /// [`Self::select_worktree`] just before it switches away.
    pub(crate) fn record_worktree_session(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.current_worktree_path() else {
            return;
        };
        if !self.session_restored.contains(&cwd) {
            return;
        }
        let tabs = self.session_tabs_for(&cwd);
        // A real disk write is a background merge against a file other Jerry instances share
        // (`TabOrderState::save_merged_at`), so it is worth *not* doing when the answer is
        // unchanged - and the unchanged case is genuinely common, since this is also called from
        // `select_worktree`'s safety-net position on every single switch.
        if self.tab_order_state.session_tabs(&cwd) == tabs {
            return;
        }
        self.tab_order_state.set_session_tabs(&cwd, &tabs);
        if let Some(key) = worktree_key(&cwd) {
            self.tab_order_owned.insert(key);
        }
        self.persist_tab_order(cx);
    }

    /// `cwd`'s live tab strip, as the session record that would be written for it - split out of
    /// [`Self::record_worktree_session`] so the mapping from real tabs to persisted ones is one
    /// readable list rather than buried in that method's own guards.
    fn session_tabs_for(&self, cwd: &Path) -> Vec<SessionTab> {
        self.combined_tab_order()
            .iter()
            .filter_map(|tab_ref| match tab_ref {
                TabRef::File(relative) => Some(SessionTab::File(cwd.join(relative))),
                TabRef::Agent(id) => {
                    let agent = self.agents.iter().find(|agent| agent.id == *id)?;
                    Some(match agent.kind {
                        ProcessKind::Shell => SessionTab::Shell,
                        ProcessKind::Agent(kind) => SessionTab::Agent {
                            kind,
                            session_id: self.live_agent_session_id(agent, kind),
                        },
                    })
                }
                // See `Self::record_worktree_session`'s own docs for why neither is recorded.
                TabRef::Graph | TabRef::Review(_) | TabRef::Run => None,
            })
            .collect()
    }

    /// The real Claude Code `session_id` currently known for a live agent, or `None`.
    fn live_agent_session_id(
        &self,
        agent: &crate::work_surface::agents::Agent,
        kind: AgentKind,
    ) -> Option<String> {
        let key = crate::review::state::baseline_key(&agent.cwd, kind, agent.spawned_at_unix);
        self.agent_status_state.get(&key)?.session_id.clone()
    }

    /// Reopens `cwd`'s persisted tab session for real - the read side of this module, and the one
    /// place tabs are ever created from a persisted record.
    pub(crate) fn restore_worktree_session(
        &mut self,
        cwd: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cwd != self.file_tree_root {
            return;
        }
        if !self.session_restored.insert(cwd.clone()) {
            return;
        }
        // Deliberately *after* the mark above, not before it. A window with no persisted state at
        // all has, trivially, nothing to restore - and marking it restored anyway is what keeps
        // [`Self::record_worktree_session`]'s gate meaning "the live strip is the truth for this
        // worktree" rather than "this window persists things": with no file to erase, there is no
        // hazard for that gate to protect against, and blocking the recorder here would silently
        // stop maintaining the in-memory [`Self::tab_order_state`] a `None`-path window still
        // reads back through `Self::combined_tab_order`.
        if self.tab_order_path.is_none() {
            return;
        }
        let tabs = self.tab_order_state.session_tabs(&cwd);
        if tabs.is_empty() {
            return;
        }

        // Built as each tab is really restored, so a skipped entry leaves no gap behind: the
        // restored strip is exactly the tabs that genuinely came back, in their remembered
        // relative order.
        let mut order: Vec<TabRef> = Vec::new();
        for tab in tabs {
            match tab {
                SessionTab::File(absolute) => {
                    let Ok(relative) = absolute.strip_prefix(&cwd) else {
                        continue;
                    };
                    let relative = relative.to_path_buf();
                    // `is_file`, not `exists`: a path that has since become a *directory* is as
                    // unopenable as a deleted one, and opening a tab for it would leave the code
                    // surface showing a real error for a file the user never asked for.
                    if !absolute.is_file() {
                        self.note_session_restore_failure(format!(
                            "{} was open last session but no longer exists - its tab was not \
                             reopened",
                            absolute.display()
                        ));
                        continue;
                    }
                    if !self.open_files().contains(&relative) {
                        self.open_files_mut().push(relative.clone());
                    }
                    order.push(TabRef::File(relative));
                }
                SessionTab::Shell => {
                    let id = self.agents.spawn(
                        ProcessKind::Shell,
                        cwd.clone(),
                        self.settings.appearance.terminal_font_size,
                        self.settings.terminal.shell_override(),
                        // A shell, so no hook injection - `Agents::spawn` would discard one anyway.
                        None,
                        window,
                        cx,
                    );
                    // GitHub issue #225: a restored shell is a real agent like any other and needs
                    // a real review baseline too, exactly as
                    // `Self::spawn_initial_shell_for_opened_repo`'s own spawn does.
                    self.capture_review_baseline(id, cx);
                    order.push(TabRef::Agent(id));
                }
                SessionTab::Agent {
                    kind: AgentKind::Claude,
                    session_id: Some(session_id),
                } => {
                    let hook_injection = self.hook_injection_for(ProcessKind::claude());
                    let id = self.agents.spawn_resume(
                        AgentKind::Claude,
                        cwd.clone(),
                        self.settings.appearance.terminal_font_size,
                        self.settings.terminal.shell_override(),
                        hook_injection.as_ref(),
                        session_id,
                        window,
                        cx,
                    );
                    self.capture_review_baseline(id, cx);
                    order.push(TabRef::Agent(id));
                }
                SessionTab::Agent { kind, session_id } => {
                    debug_assert!(
                        session_id.is_none() || kind != AgentKind::Claude,
                        "the resumable case is handled by the arm above"
                    );
                    self.note_session_restore_failure(format!(
                        "a {} agent was open last session but has no resumable session id - its \
                         tab was not reopened (a fresh agent would not be the same conversation)",
                        kind.label()
                    ));
                }
            }
        }

        // The restored strip's own order, seeded straight into the live per-worktree order so the
        // very first render draws it as remembered. `Self::combined_tab_order`'s own persisted
        // fallback can't do this job: it only knows about file tabs (an agent's slot there is
        // decided by whichever live agent exists), and the ids these restored agents just got
        // exist only now, in this window.
        if !order.is_empty() {
            self.tab_order.insert(cwd.clone(), order);
        }
        // The restore just changed which agents exist in this worktree - re-establish "the active
        // agent always belongs to the selected worktree" and put real keyboard focus somewhere
        // that is genuinely in the rendered tree, the same pair `Agents::activate_for_worktree`'s
        // own docs require of every caller.
        self.agents.activate_for_worktree(&cwd, cx);
        self.focus_newly_spawned_agent(window, cx);
        // What was just restored *is* the session now - written straight back so a tab that could
        // not be reopened (a deleted file, an unresumable agent) stops being retried on every
        // future launch, and so the freshly-assigned agent slots are what the next restore reads.
        self.record_worktree_session(cx);
        cx.notify();
    }

    /// Records one real "this tab could not be reopened, and here is why" - logged as it happens
    /// and kept on [`Self::session_restore_notices`], oldest dropped first past
    /// [`MAX_SESSION_RESTORE_NOTICES`].
    fn note_session_restore_failure(&mut self, notice: String) {
        log::warn!("session restore: {notice}");
        self.session_restore_notices.push(notice);
        if self.session_restore_notices.len() > MAX_SESSION_RESTORE_NOTICES {
            let excess = self.session_restore_notices.len() - MAX_SESSION_RESTORE_NOTICES;
            self.session_restore_notices.drain(..excess);
        }
    }
}

/// End-to-end coverage for the real thing this module exists to do: quit Jerry, launch it again,
/// and get your tabs back.
#[cfg(test)]
mod session_restore_tests {
    use super::*;
    use crate::hooks::store::LiveRun;
    use crate::rail::status::Status;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;

    /// A real linked worktree of `repo`, created under `container` (worktrees live outside the
    /// repo - `crate::rail::repo::Repo::path`'s own docs).
    fn add_worktree(repo: &Path, container: &Path, branch: &str) -> PathBuf {
        let path = container.join(branch);
        test_support::git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        dunce::canonicalize(&path).expect("canonicalize")
    }

    /// One real launch: a fresh [`AdeApp`] against a real settings path. `repo_path: None` with
    /// `use_remembered_repo` is the real no-CLI-argument process launch (see
    /// `AdeApp::new_with_settings`'s own decision table); `Some` is a real `jerry <path>`.
    fn launch(
        cx: &mut TestAppContext,
        repo_path: Option<PathBuf>,
        settings_path: PathBuf,
    ) -> (gpui::Entity<AdeApp>, &mut gpui::VisualTestContext) {
        cx.add_window_view(|window, cx| {
            AdeApp::new_with_settings(
                repo_path,
                true,
                settings_store::Settings::default(),
                Some(settings_path),
                window,
                cx,
            )
        })
    }

    /// The index of `path` in the live worktree list, so a test can select a worktree the same way
    /// a real rail click does.
    fn worktree_index(app: &AdeApp, path: &Path) -> usize {
        app.worktrees
            .iter()
            .position(|item| item.path == path)
            .unwrap_or_else(|| {
                panic!(
                    "{} must be a real worktree row - the list is {:?}",
                    path.display(),
                    app.worktrees
                        .iter()
                        .map(|item| item.path.clone())
                        .collect::<Vec<_>>()
                )
            })
    }

    /// A worktree's tab strip reduced to something a test can assert on by *identity*, since a
    /// restored agent's own [`crate::work_surface::agents::AgentId`] is by definition a different
    /// number than the one it had last session (that is the entire reason ids are never persisted).
    #[derive(Debug, PartialEq, Eq)]
    enum Slot {
        File(String),
        Shell,
        Claude,
    }

    fn strip(app: &AdeApp) -> Vec<Slot> {
        app.combined_tab_order()
            .iter()
            .filter_map(|tab_ref| match tab_ref {
                TabRef::File(path) => Some(Slot::File(path.to_string_lossy().into_owned())),
                TabRef::Agent(id) => {
                    let agent = app.agents.iter().find(|agent| agent.id == *id)?;
                    Some(match agent.kind {
                        ProcessKind::Shell => Slot::Shell,
                        ProcessKind::Agent(AgentKind::Claude) => Slot::Claude,
                        ProcessKind::Agent(AgentKind::Codex | AgentKind::Cursor) => {
                            panic!("no test here spawns a Codex or Cursor agent into a live strip")
                        }
                    })
                }
                TabRef::Graph | TabRef::Review(_) | TabRef::Run => None,
            })
            .collect()
    }

    #[gpui::test]
    fn a_relaunch_reopens_every_file_and_terminal_tab_in_its_real_drag_order(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let container = crate::test_support::temp_root();
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");

        std::fs::write(repo_path.join("a.txt"), "a\n").expect("write");
        std::fs::write(repo_path.join("b.txt"), "b\n").expect("write");
        std::fs::write(feature.join("c.txt"), "c\n").expect("write");

        // ---- Launch 1: really open some tabs, and really drag one of them. ----
        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();

            app.update_in(cx, |app, window, cx| {
                app.open_file_view(repo_path.join("a.txt"), window, cx);
                app.open_file_view(repo_path.join("b.txt"), window, cx);
                app.new_agent(ProcessKind::Shell, window, cx);
            });
            cx.run_until_parked();
            // A real drag, dropping the second terminal in between the two file tabs - so the
            // recorded order is one no per-kind reconstruction ("every agent, then every file", or
            // the reverse) could ever produce by accident.
            app.update(cx, |app, cx| {
                let dragged = app
                    .combined_tab_order()
                    .into_iter()
                    .rfind(|tab| matches!(tab, TabRef::Agent(_)))
                    .expect("the second terminal");
                app.reorder_tab(dragged, TabRef::File(PathBuf::from("b.txt")), false, cx);
            });
            cx.run_until_parked();
            assert_eq!(
                app.read_with(cx, |app, _| strip(app)),
                vec![
                    Slot::Shell,
                    Slot::File("a.txt".to_string()),
                    Slot::Shell,
                    Slot::File("b.txt".to_string()),
                ],
                "premise: launch 1 really has this interleaved strip in the main worktree"
            );

            app.update_in(cx, |app, window, cx| {
                let index = worktree_index(app, &feature);
                app.select_worktree(index, window, cx);
                app.open_file_view(feature.join("c.txt"), window, cx);
            });
            cx.run_until_parked();
        }

        // ---- Launch 2: a genuinely fresh app against the same real settings directory. ----
        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| strip(app)),
            vec![
                Slot::Shell,
                Slot::File("a.txt".to_string()),
                Slot::Shell,
                Slot::File("b.txt".to_string()),
            ],
            "the main worktree's whole strip must come back - both file tabs, both terminals, and \
             the real drag-chosen order that interleaved them"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.iter().count()),
            2,
            "exactly the two terminals that were open - the guaranteed startup shell must not \
             stack a third one on top of a restored session that already has terminals"
        );

        app.update_in(cx, |app, window, cx| {
            let index = worktree_index(app, &feature);
            app.select_worktree(index, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| strip(app)),
            vec![Slot::File("c.txt".to_string())],
            "the other worktree's own tab must come back too, under that worktree - and only its \
             own, never the main worktree's"
        );
    }

    #[gpui::test]
    fn an_unvisited_worktrees_session_costs_nothing_until_it_is_really_selected(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let container = crate::test_support::temp_root();
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                let index = worktree_index(app, &feature);
                app.select_worktree(index, window, cx);
                // Two real, explicit terminals: switching to a worktree does not itself spawn one
                // (only *opening a repo* carries the guaranteed-initial-shell contract - see
                // `AdeApp::spawn_initial_shell_for_opened_repo`), so both of these are the user's.
                app.new_agent(ProcessKind::Shell, window, cx);
                app.new_agent(ProcessKind::Shell, window, cx);
            });
            cx.run_until_parked();
            assert_eq!(
                app.read_with(cx, |app, _| app
                    .agents
                    .iter_for_cwd(feature.clone())
                    .count()),
                2,
                "premise: the feature worktree really had two terminals when the app was quit"
            );
        }

        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app
                .agents
                .iter_for_cwd(feature.clone())
                .count()),
            0,
            "the feature worktree was not selected at launch, so not one of its processes may \
             have been spawned - restoring every worktree eagerly is exactly what this module's \
             scope decision rejects"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.agents.iter().count()),
            1,
            "only the worktree actually landed in got a real terminal"
        );

        app.update_in(cx, |app, window, cx| {
            let index = worktree_index(app, &feature);
            app.select_worktree(index, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app
                .agents
                .iter_for_cwd(feature.clone())
                .count()),
            2,
            "and going there really does bring both of its terminals back"
        );
    }

    #[gpui::test]
    fn a_file_deleted_between_sessions_is_skipped_with_a_real_reason(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");
        std::fs::write(repo_path.join("kept.txt"), "kept\n").expect("write");
        std::fs::write(repo_path.join("gone.txt"), "gone\n").expect("write");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(repo_path.join("gone.txt"), window, cx);
                app.open_file_view(repo_path.join("kept.txt"), window, cx);
            });
            cx.run_until_parked();
        }

        // The real thing that happens between sessions: the file is deleted (or moved, or renamed
        // by a branch switch) while Jerry isn't running.
        std::fs::remove_file(repo_path.join("gone.txt")).expect("remove");

        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![PathBuf::from("kept.txt")],
            "the surviving file must still reopen - one missing file may not cost the user the \
             rest of the session"
        );
        let notices = app.read_with(cx, |app, _| app.session_restore_notices.clone());
        assert_eq!(notices.len(), 1, "exactly one real refusal: {notices:?}");
        assert!(
            notices[0].contains("gone.txt") && notices[0].contains("no longer exists"),
            "the refusal must name the real file and the real reason - got {:?}",
            notices[0]
        );
    }

    #[gpui::test]
    fn a_claude_agent_is_restored_by_a_real_resume_carrying_its_own_session_id(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");
        let session_id = "5af4c210-34fa-4ab2-9c35-f6ceab76551c";

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                // `Agents::spawn` directly rather than `Self::new_agent`: the two differ only in
                // that `new_agent` would additionally bring up a real hook listener for this
                // launch, and this test supplies the hook *fact* (the session id) by hand below
                // anyway, since no real `claude` binary is running to report one.
                app.agents.spawn(
                    ProcessKind::claude(),
                    repo_path.clone(),
                    12.0,
                    None,
                    None,
                    window,
                    cx,
                );
            });
            // Exactly what `crate::hooks::flow::AdeApp::record_agent_statuses` writes when a real
            // Claude hook reports a session id, under the identical key.
            app.update(cx, |app, cx| {
                let agent = app
                    .agents
                    .iter()
                    .find(|agent| agent.kind == ProcessKind::claude())
                    .expect("the agent just spawned");
                let (cwd, spawned_at_unix) = (agent.cwd.clone(), agent.spawned_at_unix);
                let key =
                    crate::review::state::baseline_key(&cwd, AgentKind::Claude, spawned_at_unix);
                app.agent_status_state.set(
                    key,
                    LiveRun::new(&cwd, "Claude", spawned_at_unix, Status::Idle)
                        .session_id(session_id.to_owned()),
                    spawned_at_unix,
                );
                app.record_worktree_session(cx);
            });
            cx.run_until_parked();
        }

        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        let pane = app.read_with(cx, |app, _| {
            app.agents
                .iter()
                .find(|agent| agent.kind == ProcessKind::claude())
                .expect("the Claude agent must have been restored as a real tab")
                .pane
                .clone()
        });
        let spec = pane.read_with(cx, |pane, _| pane.spec_for_test().clone());
        assert_eq!(
            spec.program,
            PathBuf::from("claude"),
            "a restored agent must spawn the real claude binary"
        );
        assert_eq!(
            spec.args[0..2],
            ["--resume".to_owned(), session_id.to_owned()],
            "the restored agent must genuinely resume the same conversation it was in last \
             session - a fresh, contextless spawn in the same slot is not a restored agent"
        );
        assert_eq!(
            spec.cwd,
            repo_path.clone(),
            "and resume into the same worktree it was running in"
        );
        assert!(
            app.read_with(cx, |app, _| app.session_restore_notices.is_empty()),
            "a genuinely resumable agent must not be reported as a refusal"
        );
    }

    #[gpui::test]
    fn an_agent_with_no_resumable_session_is_refused_honestly_and_alone(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");
        std::fs::write(repo_path.join("kept.txt"), "kept\n").expect("write");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                // No hook ever reported a session id for either of these - the real state of every
                // Codex agent, and of a Claude agent that never ran a turn.
                app.agents.spawn(
                    ProcessKind::codex(),
                    repo_path.clone(),
                    12.0,
                    None,
                    None,
                    window,
                    cx,
                );
                app.agents.spawn(
                    ProcessKind::claude(),
                    repo_path.clone(),
                    12.0,
                    None,
                    None,
                    window,
                    cx,
                );
                app.open_file_view(repo_path.join("kept.txt"), window, cx);
            });
            app.update(cx, |app, cx| app.record_worktree_session(cx));
            cx.run_until_parked();
        }

        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        assert!(
            app.read_with(cx, |app, _| app
                .agents
                .iter()
                .all(|agent| agent.kind == ProcessKind::Shell)),
            "neither unresumable agent may come back as a real agent tab"
        );
        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![PathBuf::from("kept.txt")],
            "and the rest of the session must be entirely unaffected by the two refusals"
        );
        let notices = app.read_with(cx, |app, _| app.session_restore_notices.clone());
        assert_eq!(notices.len(), 2, "one refusal each: {notices:?}");
        assert!(
            notices.iter().any(|notice| notice.contains("Codex"))
                && notices.iter().any(|notice| notice.contains("Claude")),
            "each refusal must name the real agent it is about - got {notices:?}"
        );
        assert!(
            notices
                .iter()
                .all(|notice| notice.contains("no resumable session id")),
            "and the real reason, not a generic failure - got {notices:?}"
        );
    }

    #[gpui::test]
    fn relaunching_with_no_cli_argument_lands_back_in_the_last_worktree_with_its_tabs(
        cx: &mut TestAppContext,
    ) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let container = crate::test_support::temp_root();
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");
        std::fs::write(feature.join("only-here.txt"), "hi\n").expect("write");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                let index = worktree_index(app, &feature);
                app.select_worktree(index, window, cx);
                app.open_file_view(feature.join("only-here.txt"), window, cx);
            });
            cx.run_until_parked();
        }

        let (app, cx) = launch(cx, None, settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_root.clone()),
            feature,
            "the relaunch must land in the worktree that was really being worked in, not in the \
             repo's main checkout"
        );
        assert_eq!(
            app.read_with(cx, |app, _| strip(app)),
            vec![Slot::File("only-here.txt".to_string()), Slot::Shell],
            "and that worktree's own tab must be the one that came back. The trailing terminal is \
             not a restored tab - this worktree had none open (a plain worktree switch never \
             spawns one) - it is the guaranteed initial shell every *opened repo* gets, landing \
             after the restored tab exactly as a freshly spawned tab always does"
        );
    }

    #[gpui::test]
    fn an_explicitly_named_path_still_wins_over_the_remembered_worktree(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let container = crate::test_support::temp_root();
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                let index = worktree_index(app, &feature);
                app.select_worktree(index, window, cx);
            });
            cx.run_until_parked();
        }

        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_root.clone()),
            repo_path,
            "`jerry <repo>` names the repo's own checkout, and must open exactly it"
        );
    }

    #[gpui::test]
    fn both_repos_tab_sessions_survive_a_relaunch(cx: &mut TestAppContext) {
        let repo_a = crate::test_support::temp_repo();
        let repo_a = repo_a.to_path_buf();
        let repo_b = crate::test_support::temp_repo();
        let repo_b = repo_b.to_path_buf();
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");
        std::fs::write(repo_a.join("in-a.txt"), "a\n").expect("write");
        std::fs::write(repo_b.join("in-b.txt"), "b\n").expect("write");

        {
            let (app, cx) = launch(cx, Some(repo_a.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(repo_a.join("in-a.txt"), window, cx);
            });
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                app.open_repo_in_current_window(repo_b.clone(), window, cx);
            });
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(repo_b.join("in-b.txt"), window, cx);
            });
            cx.run_until_parked();
        }

        // Relaunch with no CLI argument - repo B was the last focused, so that is where this
        // lands, with its own tab.
        let (app, cx) = launch(cx, None, settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.repos.len()),
            2,
            "premise: both repos are still known after the relaunch (`repos.toml`)"
        );
        assert_eq!(
            app.read_with(cx, |app, _| strip(app)),
            vec![Slot::Shell, Slot::File("in-b.txt".to_string())],
            "the repo that was focused at quit comes back with its own tab"
        );

        // And the other repo's session comes back on the first real visit to its worktree - the
        // same lazy restore any other unvisited worktree gets, across a repo boundary.
        app.update_in(cx, |app, window, cx| {
            app.select_worktree_by_path(&repo_a, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            app.read_with(cx, |app, _| app.file_tree_root.clone()),
            repo_a,
            "premise: the click really moved to the other repo's worktree"
        );
        assert_eq!(
            app.read_with(cx, |app, _| strip(app)),
            vec![Slot::Shell, Slot::File("in-a.txt".to_string())],
            "the other repo's own tabs must come back too - 'everything reopens' spans repos, not \
             just the one that happened to be focused"
        );
    }

    #[gpui::test]
    fn a_deliberately_closed_tab_stays_closed_across_a_relaunch(cx: &mut TestAppContext) {
        let repo = crate::test_support::temp_repo();
        let repo_path = repo.to_path_buf();
        let settings_dir = crate::test_support::temp_root();
        let settings_path = settings_dir.path().join("settings.toml");
        std::fs::write(repo_path.join("kept.txt"), "kept\n").expect("write");
        std::fs::write(repo_path.join("closed.txt"), "closed\n").expect("write");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            app.update_in(cx, |app, window, cx| {
                app.open_file_view(repo_path.join("kept.txt"), window, cx);
                app.open_file_view(repo_path.join("closed.txt"), window, cx);
                app.close_file_tab(PathBuf::from("closed.txt"), window, cx);
            });
            cx.run_until_parked();
        }

        let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path);
        cx.run_until_parked();

        assert_eq!(
            app.read_with(cx, |app, _| app.open_files().to_vec()),
            vec![PathBuf::from("kept.txt")],
            "only the tab that was still open at quit may come back"
        );
        assert!(
            app.read_with(cx, |app, _| app.session_restore_notices.is_empty()),
            "and a tab that was never recorded is not a refusal - `closed.txt` still exists on \
             disk; it simply wasn't open"
        );
    }
}
