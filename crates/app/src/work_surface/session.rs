//! Recording and restoring one worktree's whole tab session - "quit Jerry with some set of tabs
//! open across some set of worktrees and repos, relaunch it, and everything you had open comes
//! back".
//!
//! [`crate::work_surface::tab_order_state`] is the durable half (what the file looks like, what
//! survives a restart, and why); this is the live half: turning the real tab strip into a
//! [`crate::work_surface::tab_order_state::SessionTab`] list on every change
//! ([`AdeApp::record_worktree_session`]), and turning that list back into real tabs, real files and
//! real processes ([`AdeApp::restore_worktree_session`]).
//!
//! ## Scope and timing: restore per worktree, on its first real activation
//!
//! The obvious design - restore everything at launch - was rejected deliberately, and this is the
//! one genuinely product-shaped decision in this module, so it is written down rather than left
//! implicit.
//!
//! A restored tab is not a row in a list; it is a real OS process. A user who has worked in a
//! dozen worktrees across three repos would, on a naive eager restore, watch Jerry fork a dozen
//! shells and resume a handful of Claude conversations - real PTYs, real CPU, real token spend on
//! conversations they did not ask to continue - before the first frame they could act on. That is
//! slow, expensive, and surprising in a way "restore my tabs" does not ask for.
//!
//! So restore is **lazy and per worktree**: a worktree's session is reopened the first time that
//! worktree is genuinely selected in this window, and never again in it (see
//! [`crate::root::AdeApp::session_restored`]). What that buys, concretely:
//!
//! - **Launch still lands you back where you were**, with your tabs, because startup genuinely
//!   selects a worktree on the way in (`crate::root::AdeApp::load_worktrees_for_opened_repo` ->
//!   `crate::rail::worktrees::selection_for_opened_repo`), and - new here - it prefers the
//!   worktree you were last in (`crate::rail::repo::RepoRecord::selected_worktree`) rather than
//!   always the main checkout. One worktree's worth of processes, not every worktree's.
//! - **Every other worktree and repo comes back the instant you go there**, at the cost of exactly
//!   the visit the user just performed anyway. From the user's side "everything reopened"; from
//!   the machine's side, nothing was spawned that was never looked at.
//! - **PR #265's invariant holds structurally rather than by care.** That change established that
//!   a tab is never shown, never spawnable, and never implicitly attributed to anything except a
//!   real, currently-selected worktree. Restore hangs off *selection itself*, so there is no path
//!   through this module that can produce a tab for a worktree that isn't selected - and
//!   [`AdeApp::restore_worktree_session`] additionally refuses outright if it is ever handed a
//!   worktree that isn't the live one.
//!
//! The honest cost of the choice: switching into a worktree you haven't visited yet this run spawns
//! its processes at that moment, so that one switch is heavier than a plain switch used to be. That
//! is the same cost the eager design pays, just moved to the moment it buys something.
//!
//! ## What restoring can and cannot honestly do
//!
//! - A **file tab** is reopened outright. A file deleted or moved since the last session is
//!   skipped with a real reason ([`crate::root::AdeApp::session_restore_notices`]) - never a
//!   phantom tab for a path that no longer resolves.
//! - A **terminal tab** is a fresh shell in the same worktree, in the same slot. A real OS shell
//!   process cannot survive an app quit and this codebase has no process-reattachment to pretend
//!   otherwise with, so that is exactly what is claimed and exactly what happens.
//! - An **agent tab** is restored only as a genuine `claude --resume <session_id>`
//!   ([`crate::work_surface::agents::Agents::spawn_resume`], GitHub issue #227's real, verified
//!   resume path) - the same conversation, carried forward. An agent with no recorded session id
//!   (every Codex agent, since no hooks exist for Codex at all, and any Claude agent that closed
//!   before a hook reported one) is **not** restored, and says why. Spawning a fresh, contextless
//!   agent into that slot would look like a restored conversation and be nothing of the kind.
//!
//! One tab degrading never fails the rest of the restore - each is decided on its own.

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
    ///
    /// Cheap enough to call that freely: it snapshots an already-in-memory tab order, compares it
    /// against what is already recorded, and returns without touching the disk at all when nothing
    /// changed - which is the common case for the safety-net call.
    ///
    /// Refuses, deliberately, in three cases:
    ///
    /// - No worktree genuinely selected ([`Self::active_agent_cwd`] is `None`). There is no such
    ///   thing as a session belonging to a repo rather than to a worktree, so there is nothing to
    ///   file it under.
    /// - This worktree's own session hasn't been restored yet
    ///   ([`Self::session_restored`]). Until it has, the live tab strip is *not* the truth about
    ///   this worktree - it is whatever exists before the restore that was about to run - and
    ///   writing it would erase the session the next selection was going to reopen. This is the
    ///   real ordering hazard in the whole feature, and this check is where it is closed.
    /// - `graph`/`review` tabs are dropped from the recorded session rather than persisted. Both
    ///   are single, window-wide slots ([`Self::graph_tab_open`], [`Self::review_tab_open`]) rather
    ///   than per-worktree tabs, and a review tab additionally names a live
    ///   [`crate::work_surface::agents::AgentId`] that no restart can resolve; recording either
    ///   would mean inventing a per-worktree existence neither actually has. They simply reopen
    ///   the way they always have, from their own live state.
    pub(crate) fn record_worktree_session(&mut self, cx: &mut Context<Self>) {
        let Some(cwd) = self.active_agent_cwd() else {
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
    ///
    /// Reads [`Self::combined_tab_order`], not `Agents`/[`Self::open_files`] directly, so the
    /// recorded order is by construction the order the user actually sees - including a
    /// drag-chosen interleaving of agents and files, which is the whole thing issue #16 persists
    /// and which reading the two underlying lists separately would silently flatten back into
    /// "agents, then files".
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
                TabRef::Graph | TabRef::Review(_) => None,
            })
            .collect()
    }

    /// The real Claude Code `session_id` currently known for a live agent, or `None`.
    ///
    /// Deliberately read out of [`Self::agent_status_state`] - GitHub issue #239 phase 2's
    /// hook-learned facts, keyed by `crate::review::state::baseline_key` - rather than tracked as a
    /// second copy on [`crate::work_surface::agents::Agent`]. That store is already the one place
    /// a session id is ever learned (an agent's own hooks report it), already keyed by exactly the
    /// three durable facts available here, and already what
    /// `crate::hooks::flow::AdeApp::resume_past_agent` resumes from - so a session recorded here
    /// and a session resumed from the rail's history rows can never disagree about what a given
    /// agent's conversation is.
    ///
    /// `None` is a real and common answer, not a failure: a Codex agent has no hooks at all, and a
    /// Claude agent that has not yet run a single turn has not reported one yet.
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
    ///
    /// Called from exactly the two places a worktree becomes genuinely, currently selected:
    /// [`Self::select_worktree`] (every rail click, and every programmatic selection that goes
    /// through it) and [`Self::spawn_initial_shell_for_opened_repo`] (a just-opened repo, which
    /// also covers `active_agent_cwd`'s one documented no-usable-worktree last resort). Ordering
    /// matters at the second: this runs *before* that method's guaranteed initial shell, so a
    /// worktree whose session already contains a terminal gets its own remembered one back rather
    /// than that one plus a redundant fresh one - the "already has an agent" check there does the
    /// rest.
    ///
    /// Refuses without marking anything when it is handed a worktree that isn't the live one
    /// (`cwd != self.file_tree_root`): the file tabs it reopens are stored per worktree keyed by
    /// exactly that root ([`Self::open_files_mut`]), so restoring against a stale root would file
    /// one worktree's tabs under another. Refusing rather than marking is what lets the real,
    /// correctly-ordered call a moment later still do the work.
    ///
    /// A no-op for a window with no real persistence at all ([`Self::tab_order_path`] is `None` -
    /// every GPUI test that hasn't opted into a real settings path), and for a worktree with
    /// nothing recorded. Neither is an error: a worktree Jerry has never seen has no session, and
    /// opens exactly as it always has.
    ///
    /// One selection path deliberately doesn't reach here: `Self::load_worktrees`'s own
    /// fall-back-to-main recovery, which re-points [`Self::selected`] from a background task with
    /// no `&mut Window` to move focus (or spawn anything) with - see that arm's own docs. A
    /// worktree reached that way keeps its recorded session untouched (the recorder's gate refuses
    /// a worktree that hasn't been through here) and restores it on the next real selection, which
    /// is the honest outcome: an external `git worktree remove` is not the user asking to reopen
    /// somewhere.
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
///
/// Every test here performs a genuine relaunch - a second, independently constructed [`AdeApp`]
/// against the *same* real settings directory, exactly as a real process restart does - rather
/// than reading the persisted file back and asserting on its contents. The file format already has
/// its own unit coverage in `crate::work_surface::tab_order_state`; what these prove is that the
/// live app really writes it, really reads it, and really reopens what it names.
#[cfg(test)]
mod session_restore_tests {
    use super::*;
    use crate::rail::status::Status;
    use crate::settings::store as settings_store;
    use gpui::TestAppContext;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("failed to spawn git");
        assert!(output.status.success(), "git {args:?} failed in {dir:?}");
    }

    /// A real repo with a real initial commit, so `git worktree list --porcelain` reports a real
    /// main-worktree row for the app to select - and so `git worktree add` below works at all.
    /// Returns the *canonicalized* path, because every one of this app's per-worktree lookups is an
    /// exact comparison against git's own fully-resolved answers (see
    /// `crate::rail::repo::canonical_repo_path`'s own docs).
    fn init_repo() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
        git(dir.path(), &["add", "README.md"]);
        git(dir.path(), &["commit", "-m", "initial"]);
        let canonical = std::fs::canonicalize(dir.path()).expect("canonicalize");
        (dir, canonical)
    }

    /// A real linked worktree of `repo`, created under `container` (worktrees live outside the
    /// repo - `crate::rail::repo::Repo::path`'s own docs).
    fn add_worktree(repo: &Path, container: &Path, branch: &str) -> PathBuf {
        let path = container.join(branch);
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                path.to_str().expect("utf8 path"),
            ],
        );
        std::fs::canonicalize(&path).expect("canonicalize")
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
                        ProcessKind::Agent(AgentKind::Codex) => {
                            panic!("no test here spawns a Codex agent into a live strip")
                        }
                    })
                }
                TabRef::Graph | TabRef::Review(_) => None,
            })
            .collect()
    }

    /// The headline behaviour, end to end: file tabs *and* terminal tabs, in a real drag-chosen
    /// interleaved order, across two real worktrees - quit, relaunch, and get every one of them
    /// back, in the same order, under the same worktree.
    ///
    /// The interleaving matters specifically: reconstructing "all the agents, then all the files"
    /// would pass a naive per-kind assertion while silently losing the one thing GitHub issue #16's
    /// drag order exists to record.
    #[gpui::test]
    fn a_relaunch_reopens_every_file_and_terminal_tab_in_its_real_drag_order(
        cx: &mut TestAppContext,
    ) {
        let (_repo, repo_path) = init_repo();
        let container = TempDir::new().expect("tempdir");
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = TempDir::new().expect("tempdir");
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
                // A second terminal alongside the one every window starts with.
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

            // A second worktree, with its own separate tab.
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

        // The second worktree's own session comes back on its first real selection, not at launch.
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

    /// The scope/timing decision made observable: a worktree the user has not gone to yet this
    /// launch has spawned nothing at all. Restoring every worktree eagerly would show up here as
    /// the feature worktree's terminal already running before it was ever selected.
    #[gpui::test]
    fn an_unvisited_worktrees_session_costs_nothing_until_it_is_really_selected(
        cx: &mut TestAppContext,
    ) {
        let (_repo, repo_path) = init_repo();
        let container = TempDir::new().expect("tempdir");
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = TempDir::new().expect("tempdir");
        let settings_path = settings_dir.path().join("settings.toml");

        {
            let (app, cx) = launch(cx, Some(repo_path.clone()), settings_path.clone());
            cx.run_until_parked();
            // Give the feature worktree two real terminals of its own.
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

    /// A file deleted between sessions must degrade to exactly one missing tab, with a real
    /// reason - never a panic, and never a phantom tab for a path that no longer resolves.
    #[gpui::test]
    fn a_file_deleted_between_sessions_is_skipped_with_a_real_reason(cx: &mut TestAppContext) {
        let (_repo, repo_path) = init_repo();
        let settings_dir = TempDir::new().expect("tempdir");
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

    /// The agent half, and the part that makes it worth doing at all: a Claude agent whose real
    /// `session_id` was captured last session comes back as a genuine
    /// `claude --resume <session_id>` - the same conversation, not a fresh one in the same slot.
    ///
    /// Asserted against the restored pane's real [`crate::terminal::pane::TerminalSpec`], mirroring
    /// `crate::work_surface::render`'s own `spawn_resume_prepends_resume_ahead_of_the_real_hook_
    /// injection` - the same way GitHub issue #227's resume proves continuity, since the argument
    /// list is what the `claude` binary itself acts on.
    #[gpui::test]
    fn a_claude_agent_is_restored_by_a_real_resume_carrying_its_own_session_id(
        cx: &mut TestAppContext,
    ) {
        let (_repo, repo_path) = init_repo();
        let settings_dir = TempDir::new().expect("tempdir");
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
                    &cwd,
                    "Claude",
                    spawned_at_unix,
                    Status::Idle,
                    None,
                    None,
                    Some(session_id.to_owned()),
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

    /// The honest failure the request asks for: an agent with no resumable session id (every Codex
    /// agent - no hooks exist for it at all - and any Claude agent that closed before one was ever
    /// reported) is **not** restored, and says why. Spawning a fresh agent into that slot would
    /// look like a restored conversation and be nothing of the kind.
    ///
    /// The rest of the session must survive the refusal - that is the "degrade a single tab, don't
    /// fail the whole restore" half.
    #[gpui::test]
    fn an_agent_with_no_resumable_session_is_refused_honestly_and_alone(cx: &mut TestAppContext) {
        let (_repo, repo_path) = init_repo();
        let settings_dir = TempDir::new().expect("tempdir");
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

    /// "Relaunch Jerry" (no CLI argument at all - the real process-launch path) must land back in
    /// the worktree you were actually last working in, with its tabs, rather than in the main
    /// checkout with someone else's. Without the per-repo
    /// [`crate::rail::repo::RepoRecord::selected_worktree`] memory this whole feature would only
    /// pay off after the user manually clicked the right rail row.
    #[gpui::test]
    fn relaunching_with_no_cli_argument_lands_back_in_the_last_worktree_with_its_tabs(
        cx: &mut TestAppContext,
    ) {
        let (_repo, repo_path) = init_repo();
        let container = TempDir::new().expect("tempdir");
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = TempDir::new().expect("tempdir");
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

        // No CLI argument: the real "just launch Jerry again" gesture.
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

    /// An explicitly named path is a real statement about where to work, and must win over the
    /// remembered worktree - the deliberate other half of the decision the previous test covers.
    /// Silently opening a different worktree because it was the last one visited would be
    /// overriding the user, not helping them.
    #[gpui::test]
    fn an_explicitly_named_path_still_wins_over_the_remembered_worktree(cx: &mut TestAppContext) {
        let (_repo, repo_path) = init_repo();
        let container = TempDir::new().expect("tempdir");
        let feature = add_worktree(&repo_path, container.path(), "feature");
        let settings_dir = TempDir::new().expect("tempdir");
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

    /// The multi-repo reading of "everything should reopen": quit with two real repos open, each
    /// with its own tabs, relaunch, and both are back - the focused one immediately, the other the
    /// moment the user goes to it.
    #[gpui::test]
    fn both_repos_tab_sessions_survive_a_relaunch(cx: &mut TestAppContext) {
        let (_repo_a, repo_a) = init_repo();
        let (_repo_b, repo_b) = init_repo();
        let settings_dir = TempDir::new().expect("tempdir");
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
            // A real second repo, opened in the same window exactly as "Open Folder…" does.
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

    /// The other direction, and the reason the recorder is wired into every close path: a tab the
    /// user deliberately closed must stay closed across a relaunch. A restore that only ever added
    /// tabs would quietly resurrect them forever.
    #[gpui::test]
    fn a_deliberately_closed_tab_stays_closed_across_a_relaunch(cx: &mut TestAppContext) {
        let (_repo, repo_path) = init_repo();
        let settings_dir = TempDir::new().expect("tempdir");
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
