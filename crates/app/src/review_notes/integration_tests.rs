//! GitHub issue #288, end to end in a real window: the mock's own acceptance sequence, the
//! resolution of *which agent* a file's notes go to, real delivery into that agent's real pty, and
//! the two properties the whole feature rests on (a note survives, and sending never clears one).
//!
//! Everything below is built from real parts - a real git repo with a real diff, a real agent tab
//! with a real child process on a real pty, real clicks and real keystrokes - and measured against
//! real painted bounds and real terminal output.

use super::render::notes_bar_label;
use super::{FileNoteState, NoteAnchor};
use crate::provenance::{store::ProvenanceStore, AgentKey};
use crate::root::focus::palette_focus_tests::open_test_app;
use crate::root::AdeApp;
use crate::sidebar::render::RightSidebarView;
use crate::work_surface::agents::{AgentId, AgentKind, ProcessKind};
use gpui::TestAppContext;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

const PATH: &str = "src/api/users.rs";

/// As committed.
const BASE: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let sql = self.orm.select(&[\"id\", \"email\"]);
    }
}
";

/// After an agent rewrote the body - one real removed line and one real added line.
const AFTER: &str = "\
impl UserApi {
    pub async fn list(&self, page: Page) -> Result<Vec<User>> {
        let q = QueryBuilder::table(\"users\").select(&[\"id\", \"email\"]);
    }
}
";

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A real repo with a real one-line change in `src/api/users.rs`, plus provenance recording that
/// one real agent wrote it.
fn repo_with_an_agent_authored_change(spawned_at: i64) -> (TempDir, ProvenanceStore) {
    let dir = TempDir::new().expect("tempdir");
    let repo = dir.path();
    git(repo, &["init", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test User"]);
    std::fs::create_dir_all(repo.join("src/api")).expect("mkdir");
    std::fs::write(repo.join(PATH), BASE).expect("seed");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "initial"]);

    let file = repo.join(PATH);
    let mut store = ProvenanceStore::default();
    let key = AgentKey::new(crate::review::state::baseline_key(
        repo,
        AgentKind::Claude,
        spawned_at,
    ));
    store.begin_agent_edit(repo, &file);
    std::fs::write(&file, AFTER).expect("the agent's own write");
    store.record_agent_edit(repo, &file, &key);
    (dir, store)
}

/// A real, executable stand-in for an agent CLI: it turns bracketed paste on (exactly as a real
/// agent's full-screen TUI does) and then writes back whatever is typed into it.
///
/// Real, not simulated. It is spawned through this app's own spawn path onto a real pty, so the
/// prompt appearing in its pane is round-tripped evidence that the bytes genuinely left the app.
///
/// `stty -echo` is load-bearing, not tidiness: with the pty's own line-discipline echo left on,
/// every delivered byte would appear on the pane **twice** (once echoed by the driver, once
/// written by `cat`), and "how many prompts arrived" - the one thing
/// [`two_notes_are_delivered_as_one_prompt_not_two`] exists to measure - would be uncountable.
/// The same technique `crate::terminal::pane`'s own
/// `shutdown_joins_reader_thread_even_with_local_echo_disabled` already uses.
fn agent_stand_in(dir: &Path) -> String {
    let script = dir.join("agent-stand-in.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nstty -echo\nprintf '\\033[?2004h'\nexec cat\n",
    )
    .expect("write shim");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod +x");
    }
    script.to_string_lossy().into_owned()
}

/// Opens the app on `repo` with the Changes panel showing and `PATH`'s diff open, plus one real
/// agent tab whose child is [`agent_stand_in`].
///
/// The tab is spawned as a `Shell` (which is the one spawn path that lets a test name the real
/// program) and then retagged to a real agent kind through `Agents::set_kind_for_test` - the
/// established seam this crate already uses for exactly this ("everything downstream of the
/// recorded `kind` field behaves exactly as if a real agent CLI had been spawned, without the
/// process weight of one"). The *process* is real either way, which is the half this test needs.
fn open_review<'a>(
    cx: &'a mut TestAppContext,
    repo: &TempDir,
    shim_dir: &TempDir,
    store: ProvenanceStore,
    spawned_at: i64,
) -> (
    gpui::Entity<AdeApp>,
    &'a mut gpui::VisualTestContext,
    AgentId,
) {
    let program = agent_stand_in(shim_dir.path());
    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
    let id = app.update_in(cx, |app, window, cx| {
        app.settings.terminal.shell = Some(program);
        app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        app.new_agent(ProcessKind::Shell, window, cx);
        app.agents.active_id().expect("the tab we just spawned")
    });
    cx.run_until_parked();
    app.update_in(cx, |app, window, cx| {
        // Retagged to a real agent kind, and to the exact spawn second the provenance store
        // recorded, so the file's own author really resolves to this live tab.
        app.agents.set_kind_for_test(id, ProcessKind::claude());
        app.agents.set_spawned_at_unix_for_test(id, spawned_at);
        app.line_provenance = store;
        app.rebuild_change_set();
        app.open_change_diff(PathBuf::from(PATH), window, cx);
    });
    cx.run_until_parked();
    (app, cx, id)
}

/// Pumps the real poll loop until `ready`, mixing virtual-clock advance with a real sleep - the
/// same discipline `crate::work_surface::render`'s `wait_for_real_pty_output` uses, and for the
/// same reason: the pty reader is a real OS thread.
fn pump_until(
    cx: &mut gpui::VisualTestContext,
    mut ready: impl FnMut(&mut gpui::VisualTestContext) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        // The pane's own real poll interval (8ms); named here rather than imported so this
        // test module needs nothing widened in `crate::terminal::pane` for its sake.
        cx.background_executor
            .advance_clock(Duration::from_millis(8));
        cx.run_until_parked();
        if ready(cx) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn agent_screen(
    app: &gpui::Entity<AdeApp>,
    cx: &mut gpui::VisualTestContext,
    id: AgentId,
) -> String {
    let pane = app.read_with(cx, |app, _| {
        app.agents
            .iter()
            .find(|agent| agent.id == id)
            .expect("the agent")
            .pane
            .clone()
    });
    pane.read_with(cx, |pane, _| pane.visible_text_lines().join("\n"))
}

/// Clicks a diff line by its row selector and lets the app settle.
fn click_line(cx: &mut gpui::VisualTestContext, row: usize) {
    let selector: &'static str = Box::leak(format!("diff-line-{row}").into_boxed_str());
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} must really paint"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    cx.run_until_parked();
}

/// **The acceptance sequence, exactly as `STAGE-A-CHANGELOG.md` §3 states it:** *"Note pins on
/// click, bar reads `1 note on this file`, send → `1 note sent — awaiting revision` + `✓ sent`,
/// card mark `draft → sent`, note still pinned."*
///
/// Every step is a real gesture on a real painted element, and the send really writes into a real
/// child process's real pty.
#[gpui::test]
fn the_mocks_acceptance_sequence_runs_end_to_end_against_a_real_agent(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    // Before any note there is no bar at all - not an empty one.
    assert!(
        cx.debug_bounds("diff-notes-bar").is_none(),
        "a file with no notes shows no notes bar"
    );

    // 1. A note pins on click.
    click_line(cx, 2);
    assert!(
        cx.debug_bounds("diff-note-0").is_some(),
        "clicking a diff line must pin a card beneath it, as a real row of the diff's own list"
    );
    cx.simulate_input("cache key must include tenant_id");
    cx.run_until_parked();

    // 2. The bar reads `1 note on this file`.
    assert!(
        cx.debug_bounds("diff-notes-bar-label").is_some(),
        "the notes bar must really paint above the hunks"
    );
    let (state, anchor) = app.read_with(cx, |app, _| {
        let worktree = app.review_notes_worktree();
        (
            app.review_notes.file_state(&worktree, Path::new(PATH)),
            app.review_notes
                .anchors(&worktree, Path::new(PATH))
                .first()
                .copied()
                .expect("one anchor"),
        )
    });
    assert_eq!(
        notes_bar_label(state),
        "1 note on this file",
        "the design's own wording, through the pluralisation helper"
    );
    assert_eq!(
        state,
        FileNoteState {
            count: 1,
            all_sent: false
        }
    );

    // 3. Send.
    let send = cx
        .debug_bounds("diff-notes-send")
        .expect("the `Send notes to <agent>` button must really paint");
    cx.simulate_click(send.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    // 4. The bar flips, with the `✓ sent` confirmation.
    let after = app.read_with(cx, |app, _| {
        app.review_notes
            .file_state(&app.review_notes_worktree(), Path::new(PATH))
    });
    assert_eq!(
        notes_bar_label(after),
        "1 note sent \u{2014} awaiting revision",
        "the bar must say the revision is awaited, verbatim"
    );
    assert!(
        cx.debug_bounds("diff-notes-bar-sent").is_some(),
        "and the `\u{2713} sent` confirmation must really paint"
    );
    assert!(
        cx.debug_bounds("diff-notes-bar-error").is_none(),
        "a send that really landed must not also be reporting a failure"
    );

    // 5. The card's mark flipped `draft -> sent`, and the note is **still pinned**.
    app.read_with(cx, |app, _| {
        let note = app
            .review_notes
            .note(&app.review_notes_worktree(), Path::new(PATH), anchor)
            .expect("the note is still pinned after sending - it is never cleared");
        assert_eq!(note.mark(), super::NoteMark::Sent);
        assert_eq!(note.text, "cache key must include tenant_id");
    });
    assert!(
        cx.debug_bounds("diff-note-0").is_some(),
        "the card must still be a painted row of the diff after the send"
    );
    assert!(
        cx.debug_bounds("diff-note-0-mark").is_some(),
        "including its own draft/sent mark"
    );

    // 6. And the prompt really arrived in the agent's real pty.
    assert!(
        pump_until(cx, |cx| agent_screen(&app, cx, agent)
            .contains("cache key must include tenant_id")),
        "the batched prompt must really reach the target agent's child process - got:\n{}",
        agent_screen(&app, cx, agent)
    );
    let screen = agent_screen(&app, cx, agent);
    assert!(
        screen.contains("Review notes on src/api/users.rs"),
        "including the batch's own line-anchored header - got:\n{screen}"
    );
}

/// The rule the audit calls out as the thing Orca learned the hard way: **one** prompt, however
/// many notes. Two notes on one file must produce one delivery containing both, never two.
#[gpui::test]
fn two_notes_are_delivered_as_one_prompt_not_two(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    cx.simulate_input("ade-note-alpha");
    cx.run_until_parked();
    click_line(cx, 3);
    cx.simulate_input("ade-note-beta");
    cx.run_until_parked();

    let prompt = app
        .read_with(cx, |app, _| app.batched_review_prompt(Path::new(PATH)))
        .expect("two real notes compose one prompt");
    assert_eq!(prompt.note_count, 2);
    assert_eq!(
        prompt.lines().len(),
        4,
        "one header, one line per note, one closing instruction - a single prompt, not two"
    );

    let send = cx.debug_bounds("diff-notes-send").expect("the send button");
    cx.simulate_click(send.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    assert!(
        pump_until(cx, |cx| {
            let screen = agent_screen(&app, cx, agent);
            screen.contains("ade-note-alpha") && screen.contains("ade-note-beta")
        }),
        "both notes must arrive, in the same delivery - got:\n{}",
        agent_screen(&app, cx, agent)
    );
    // One prompt means one header. Two sends would have produced two.
    let screen = agent_screen(&app, cx, agent);
    assert_eq!(
        screen.matches("Review notes on").count(),
        1,
        "exactly one batched prompt reached the agent - got:\n{screen}"
    );
}

/// *"Editing after send starts a new draft state for that note"*, observed through the surface
/// rather than only through the model: the bar goes back to counting notes, and the card's mark
/// goes back to `draft`, while the note itself stays pinned.
#[gpui::test]
fn editing_a_sent_note_puts_the_card_and_the_bar_back_into_draft(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    cx.simulate_input("first wording");
    cx.run_until_parked();
    let send = cx.debug_bounds("diff-notes-send").expect("the send button");
    cx.simulate_click(send.center(), gpui::Modifiers::none());
    cx.run_until_parked();
    assert!(cx.debug_bounds("diff-notes-bar-sent").is_some());

    // Type one more character into the same card.
    let card = cx.debug_bounds("diff-note-0").expect("the pinned card");
    cx.simulate_click(card.center(), gpui::Modifiers::none());
    cx.simulate_input("!");
    cx.run_until_parked();

    let state = app.read_with(cx, |app, _| {
        app.review_notes
            .file_state(&app.review_notes_worktree(), Path::new(PATH))
    });
    assert_eq!(
        notes_bar_label(state),
        "1 note on this file",
        "the agent has not seen this wording, so the bar must stop claiming a revision is awaited"
    );
    assert!(
        cx.debug_bounds("diff-notes-bar-sent").is_none(),
        "and the `\u{2713} sent` confirmation must go with it"
    );
    app.read_with(cx, |app, _| {
        let worktree = app.review_notes_worktree();
        let anchor = app.review_notes.anchors(&worktree, Path::new(PATH))[0];
        let note = app
            .review_notes
            .note(&worktree, Path::new(PATH), anchor)
            .expect("still pinned");
        assert_eq!(note.mark(), super::NoteMark::Draft);
        assert_eq!(
            note.superseded_text(),
            Some("first wording"),
            "and what the agent really was told must still be recoverable"
        );
    });
}

/// *"`noteTarget` resolves to the file's first author"* - the live agent whose key the provenance
/// store recorded against this file, not merely whichever tab happens to be open.
#[gpui::test]
fn the_target_is_the_files_own_author_when_one_is_live(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, authored_by) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    // A second, later agent in the same worktree, which the file names nowhere.
    let second = app.update_in(cx, |app, window, cx| {
        app.new_agent(ProcessKind::Shell, window, cx);
        let id = app.agents.active_id().expect("the second tab");
        app.agents.set_kind_for_test(id, ProcessKind::codex());
        app.agents.set_spawned_at_unix_for_test(id, 1_700_000_900);
        id
    });
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        let target = app
            .review_note_target(Path::new(PATH))
            .expect("a target must resolve");
        assert_eq!(
            target.agent, authored_by,
            "in a shared worktree the notes go to whoever wrote the lines, not to the tab that \
             happens to be active"
        );
        assert!(target.from_file_author);
        assert_ne!(target.agent, second);
    });
}

/// The fallback half: a file this worktree has no attribution for still has a target - *"falling
/// back to the worktree's primary agent"*.
#[gpui::test]
fn a_file_with_no_author_falls_back_to_the_worktrees_primary_agent(cx: &mut TestAppContext) {
    let (repo, _store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    // Deliberately no provenance installed, so the file names nobody at all.
    let (app, cx, agent) = open_review(cx, &repo, &shim_dir, ProvenanceStore::default(), 1);

    app.read_with(cx, |app, _| {
        let target = app
            .review_note_target(Path::new(PATH))
            .expect("the worktree's primary agent is still a target");
        assert_eq!(target.agent, agent);
        assert!(
            !target.from_file_author,
            "and the button's tooltip must be able to say so"
        );
    });
}

/// A shell shares the worktree but authors nothing and cannot revise anything, so it is never a
/// send target - and with no real agent open at all there is no button rather than one that names
/// a target it does not have.
#[gpui::test]
fn a_worktree_whose_only_tab_is_a_shell_offers_no_send_target(cx: &mut TestAppContext) {
    let (repo, _store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let program = agent_stand_in(shim_dir.path());
    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
    app.update_in(cx, |app, window, cx| {
        app.settings.terminal.shell = Some(program);
        app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        app.new_agent(ProcessKind::Shell, window, cx);
        app.open_change_diff(PathBuf::from(PATH), window, cx);
    });
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        assert!(
            app.review_note_target(Path::new(PATH)).is_none(),
            "a shell is not somebody who can revise code"
        );
    });

    click_line(cx, 2);
    cx.simulate_input("nobody to send this to");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-notes-bar").is_some(),
        "the note still pins and the bar still counts it"
    );
    assert!(
        cx.debug_bounds("diff-notes-send").is_none(),
        "but there is no send button, rather than one naming an agent that is not there"
    );
}

/// *"Notes are keyed per worktree + path + line and survive scrolling/reopening."* Opening another
/// file and coming back must find the note exactly where it was, with its state intact.
#[gpui::test]
fn a_note_survives_leaving_the_file_and_coming_back(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    cx.simulate_input("still here later");
    cx.run_until_parked();
    let send = cx.debug_bounds("diff-notes-send").expect("the send button");
    cx.simulate_click(send.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    // Away to a file with no notes at all...
    app.update_in(cx, |app, window, cx| {
        app.open_change_diff(PathBuf::from("nonexistent.rs"), window, cx);
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-notes-bar").is_none(),
        "another file's diff must not carry this file's notes"
    );

    // ...and back.
    app.update_in(cx, |app, window, cx| {
        app.open_change_diff(PathBuf::from(PATH), window, cx);
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-note-0").is_some(),
        "the note must be pinned again on the line it was anchored to"
    );
    let state = app.read_with(cx, |app, _| {
        app.review_notes
            .file_state(&app.review_notes_worktree(), Path::new(PATH))
    });
    assert_eq!(
        notes_bar_label(state),
        "1 note sent \u{2014} awaiting revision",
        "with its sent state intact"
    );
}

/// A card opened by a stray click and never written into is not a note: clicking the same line
/// again takes it away, and nothing about it is ever counted or delivered.
#[gpui::test]
fn a_blank_card_toggles_away_again_but_a_written_one_stays(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (_app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    assert!(cx.debug_bounds("diff-note-0").is_some());
    assert!(
        cx.debug_bounds("diff-notes-bar").is_none(),
        "a blank card is a click, not a note - it is not counted"
    );

    click_line(cx, 2);
    assert!(
        cx.debug_bounds("diff-note-0").is_none(),
        "clicking the same line again takes the blank card away"
    );

    click_line(cx, 2);
    cx.simulate_input("real note");
    cx.run_until_parked();
    click_line(cx, 2);
    assert!(
        cx.debug_bounds("diff-note-0").is_some(),
        "but a card that has been written into is never destroyed by a click - it closes, and \
         stays pinned"
    );
}

/// The `mod+enter` keycaps the bar draws are a real binding, not decoration (ride-along I10).
#[gpui::test]
fn the_send_shortcut_the_bar_draws_really_sends(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    cx.simulate_input("ade-note-by-keystroke");
    cx.run_until_parked();

    // The same spec string the keycaps are rendered from, as a real keystroke.
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    });
    cx.run_until_parked();

    let state = app.read_with(cx, |app, _| {
        app.review_notes
            .file_state(&app.review_notes_worktree(), Path::new(PATH))
    });
    assert!(
        state.all_sent,
        "the shortcut the bar advertises must really send the batch"
    );
    assert!(
        pump_until(cx, |cx| agent_screen(&app, cx, agent)
            .contains("ade-note-by-keystroke")),
        "and it must reach the same real pty the button does - got:\n{}",
        agent_screen(&app, cx, agent)
    );
}

/// `C note on line`, the footer hint's own wording, as a real binding on the note cursor.
#[gpui::test]
fn the_note_shortcut_toggles_a_note_on_the_line_the_cursor_is_on(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    // The cursor is set by clicking a line; `C` then toggles that line's note.
    click_line(cx, 2);
    let anchor = app.read_with(cx, |app, _| {
        app.note_cursor.as_ref().map(|cursor| cursor.anchor)
    });
    assert!(anchor.is_some(), "clicking a line sets the note cursor");

    // Focus has to leave the card first, or `c` is a character being typed into it - which is
    // exactly what the binding's own `&& !text-input` conjunct guarantees.
    app.update_in(cx, |app, window, cx| {
        // Closing the draft is what hands focus back to the diff container - see
        // `AdeApp::close_note_draft`.
        app.close_note_draft(window, cx);
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-note-0").is_none(),
        "the blank card went with the closed draft"
    );

    cx.simulate_keystrokes("c");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-note-0").is_some(),
        "`C` must pin a note on the line the cursor is on"
    );
}

/// A note pinned to a removed line has its own anchor, and it says so in the prompt - the half a
/// single-integer line key would have silently collapsed.
#[gpui::test]
fn a_note_on_a_removed_line_anchors_and_reads_as_removed(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    // Row 1 of this diff is the removed line (`let sql = ...`), row 2 the added one.
    let removed_row = app.read_with(cx, |app, _| {
        let file = app.open_diff_file_cache.as_ref().expect("the open diff");
        let mut row = 0usize;
        for hunk in &file.hunks {
            for line in &hunk.lines {
                if line.kind == wt_core::diff::DiffLineKind::Removed {
                    return row;
                }
                row += 1;
            }
        }
        panic!("this seed must produce a real removed line");
    });
    click_line(cx, removed_row);
    cx.simulate_input("why did this go?");
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        let anchors = app
            .review_notes
            .anchors(&app.review_notes_worktree(), Path::new(PATH));
        assert!(
            matches!(anchors.as_slice(), [NoteAnchor::Old(_)]),
            "a removed line anchors to its old-file number, got {anchors:?}"
        );
        let prompt = app
            .batched_review_prompt(Path::new(PATH))
            .expect("a prompt");
        assert!(
            prompt.lines()[1].starts_with("removed line "),
            "and the prompt says so, so the agent looks in the right column - got {:?}",
            prompt.lines()[1]
        );
    });
}

/// A note has to survive the window closing, and "written when the card closes" is not that: the
/// realistic way to lose one is to type it and then quit with the caret still in it.
///
/// So this types into a card, never closes it, and asserts the note really reached the real file
/// on disk once the debounce elapsed - and, before that, that nothing was written per keystroke.
#[gpui::test]
fn a_note_still_being_typed_reaches_the_real_file_on_its_own(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let state_dir = TempDir::new().expect("tempdir");
    let notes_file =
        super::persist_state::review_notes_path_for(&state_dir.path().join("settings.toml"));
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);
    app.update(cx, |app, _| {
        app.review_notes_path = Some(notes_file.clone());
    });

    click_line(cx, 2);
    cx.simulate_input("survives the window closing");
    cx.run_until_parked();
    assert!(
        !notes_file.exists(),
        "nothing may be written per keystroke - that would be a real fsync'd file write per \
         character"
    );

    // The debounce, and nothing else - the card is still open and still focused.
    cx.background_executor
        .advance_clock(Duration::from_millis(700));
    cx.run_until_parked();

    let mut restored = super::NoteStore::default();
    let (ok, dropped) =
        super::persist_state::ReviewNotesState::load_at(&notes_file).restore_into(&mut restored);
    assert_eq!((ok, dropped), (1, 0), "the note must be on disk by itself");
    let worktree = app.read_with(cx, |app, _| app.review_notes_worktree());
    let anchor = restored.anchors(&worktree, Path::new(PATH));
    assert_eq!(anchor.len(), 1);
    assert_eq!(
        restored
            .note(&worktree, Path::new(PATH), anchor[0])
            .expect("the note")
            .text,
        "survives the window closing"
    );
}

/// Regression: a plain letter bound over the **diff** must not be swallowed while a note card is
/// being typed into.
///
/// The note card is a real `"text-input"` node *inside* `crate::code_surface::render`'s own
/// `"diff"` node, so `v` (`ToggleChangeSeen`), `]` (`NextChangedFile`) and `space`
/// (`ToggleChangeStaged`) - all scoped `Some("diff && !file-editor")` before GitHub issue #288 -
/// were live over it, and GPUI dispatches a matching `KeyBinding` before any `on_key_down`
/// listener. Typing "survives" into a note produced "suries", `]` jumped to another file
/// mid-sentence, and a space staged the file instead of separating two words. All three bindings
/// now carry `&& !text-input`.
///
/// The typed string below is chosen to exercise every one of them: it contains a `v`, a `]`, and
/// real spaces.
#[gpui::test]
fn plain_letters_bound_over_the_diff_are_typed_into_a_note_not_swallowed(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    let seen_before = app.read_with(cx, |app, _| app.seen_files.clone());
    click_line(cx, 2);
    cx.simulate_input("verify v] here");
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        let worktree = app.review_notes_worktree();
        let anchor = app.review_notes.anchors(&worktree, Path::new(PATH))[0];
        assert_eq!(
            app.review_notes
                .note(&worktree, Path::new(PATH), anchor)
                .expect("the note")
                .text,
            "verify v] here",
            "every character typed into a note must land in the note"
        );
        assert_eq!(
            app.seen_files, seen_before,
            "and `v` must not have marked the file seen behind the caret"
        );
        assert_eq!(
            app.open_change.as_deref(),
            Some(Path::new(PATH)),
            "and `]` must not have jumped to another file mid-sentence"
        );
        assert!(
            app.staged_files.is_empty(),
            "and a space must not have staged the file instead of separating two words"
        );
    });
}

/// **Regression for the worst failure this feature can have: input silently going nowhere.**
///
/// The pinned card is a row of a virtualized `uniform_list`, so it stops being built at all once
/// it scrolls out of view. If the card were the element carrying the note's `FocusHandle`, that
/// would delete the focused node from GPUI's dispatch tree mid-sentence - and GPUI then evaluates
/// every predicate against an empty context stack, where they all short-circuit to `false`. Typed
/// characters, `mod+enter` and `c` would all stop working, with nothing on screen saying so.
///
/// So: open a note near the top of a long diff, scroll it far out of view, and keep typing.
#[gpui::test]
fn typing_into_a_note_keeps_working_after_the_card_scrolls_out_of_view(cx: &mut TestAppContext) {
    let repo = TempDir::new().expect("tempdir");
    git(repo.path(), &["init", "-b", "main"]);
    git(repo.path(), &["config", "user.email", "test@example.com"]);
    git(repo.path(), &["config", "user.name", "Test User"]);
    std::fs::write(repo.path().join("big.rs"), "fn noop() {}\n").expect("seed");
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-m", "initial"]);
    // Far more lines than any viewport, so the note's own row genuinely leaves the built range.
    let mut content = String::from("fn noop() {}\n");
    for index in 0..300 {
        content.push_str(&format!("fn generated_{index}() -> i32 {{ {index} }}\n"));
    }
    std::fs::write(repo.path().join("big.rs"), &content).expect("rewrite");

    let shim_dir = TempDir::new().expect("tempdir");
    let program = agent_stand_in(shim_dir.path());
    let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
    app.update_in(cx, |app, window, cx| {
        app.settings.terminal.shell = Some(program);
        app.set_right_sidebar_view(RightSidebarView::Changes, window, cx);
        app.new_agent(ProcessKind::Shell, window, cx);
        let id = app.agents.active_id().expect("the tab");
        app.agents.set_kind_for_test(id, ProcessKind::claude());
        app.open_change_diff(PathBuf::from("big.rs"), window, cx);
    });
    cx.run_until_parked();

    click_line(cx, 1);
    cx.simulate_input("before");
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-note-0").is_some(),
        "precondition: the card is on screen while it is being typed into"
    );

    // A deliberately huge delta - `uniform_list` clamps to its own real maximum scroll offset.
    let anchor = cx
        .debug_bounds("diff-line-1")
        .expect("the noted line must be painted before scrolling");
    cx.simulate_event(gpui::ScrollWheelEvent {
        position: anchor.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-100_000.0))),
        modifiers: gpui::Modifiers::default(),
        touch_phase: gpui::TouchPhase::Moved,
    });
    cx.run_until_parked();
    assert!(
        cx.debug_bounds("diff-note-0").is_none(),
        "precondition: the card really did stop being built - otherwise this test proves nothing"
    );

    cx.simulate_input(" and after");
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        let worktree = app.review_notes_worktree();
        let anchor = app.review_notes.anchors(&worktree, Path::new("big.rs"))[0];
        assert_eq!(
            app.review_notes
                .note(&worktree, Path::new("big.rs"), anchor)
                .expect("the note")
                .text,
            "before and after",
            "keystrokes must keep reaching the note after its card scrolled out of the list"
        );
    });

    // And the shortcuts the bar advertises must still be live, for the same reason.
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    });
    cx.run_until_parked();
    app.read_with(cx, |app, _| {
        assert!(
            app.review_notes
                .file_state(&app.review_notes_worktree(), Path::new("big.rs"))
                .all_sent,
            "`mod+enter` must still fire with the card off screen"
        );
    });
}

/// A send that really failed says so in the bar, and - critically - does **not** mark anything
/// sent. Audit item I12's whole complaint about this design is that nothing in it ever fails.
#[gpui::test]
fn a_send_to_a_dead_agent_fails_loudly_and_marks_nothing(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    cx.simulate_input("never delivered");
    cx.run_until_parked();

    // End the real child the way a user would - a real `Ctrl+C` down the real pty - and let the
    // pane's own poll loop genuinely observe the exit.
    let pane = app.read_with(cx, |app, _| {
        app.agents
            .iter()
            .find(|a| a.id == agent)
            .expect("the agent")
            .pane
            .clone()
    });
    pane.update(cx, |pane, cx| pane.interrupt(cx));
    assert!(
        pump_until(cx, |cx| pane
            .read_with(cx, |pane, _| pane.exit_status().is_some())),
        "the real child must actually have ended before this test means anything"
    );

    let send = cx.debug_bounds("diff-notes-send").expect("the send button");
    cx.simulate_click(send.center(), gpui::Modifiers::none());
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        assert!(
            !app.review_notes
                .file_state(&app.review_notes_worktree(), Path::new(PATH))
                .all_sent,
            "nothing reached anybody, so nothing may claim to have been sent - and `sent` only \
             ever reverts by editing the note, so this would be unrecoverable"
        );
        assert!(
            app.note_send_error.is_some(),
            "and the failure must be said out loud rather than swallowed"
        );
    });
    assert!(
        cx.debug_bounds("diff-notes-bar-error").is_some(),
        "the notes bar must really paint the failure"
    );
    assert!(
        cx.debug_bounds("diff-notes-bar-sent").is_none(),
        "and must not also be showing a sent confirmation"
    );
}

/// A draft opened in one worktree must never write into another checkout's notes.
///
/// `AdeApp::diff_root` is reassigned wholesale on a worktree switch, so a draft that re-read it at
/// each store call would land every subsequent keystroke under the *new* worktree's key -
/// overwriting its note on the same path and line, and persisting it there.
#[gpui::test]
fn a_draft_left_open_across_a_worktree_switch_never_writes_into_the_other_checkout(
    cx: &mut TestAppContext,
) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);

    click_line(cx, 2);
    cx.simulate_input("belongs to the first checkout");
    cx.run_until_parked();
    let (first, anchor) = app.read_with(cx, |app, _| {
        let worktree = app.review_notes_worktree();
        let anchor = app.review_notes.anchors(&worktree, Path::new(PATH))[0];
        (worktree, anchor)
    });

    // The switch itself, at the one field that defines "which checkout these notes belong to".
    let second = repo.path().join("..").join("other-checkout");
    app.update(cx, |app, cx| {
        app.diff_root = second.clone();
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_input(" - typed after the switch");
    cx.run_until_parked();

    app.read_with(cx, |app, _| {
        assert!(
            app.review_notes.file(&second, Path::new(PATH)).is_none(),
            "not one character may land in the checkout the draft does not belong to"
        );
        assert_eq!(
            app.review_notes
                .note(&first, Path::new(PATH), anchor)
                .expect("the draft's own note")
                .text,
            "belongs to the first checkout - typed after the switch",
            "and the draft keeps writing where it was opened"
        );
    });
}

/// A second window's notes must survive this one saving.
///
/// `save_merged_at` rewrites every worktree key this window claims **ownership** of, from its own
/// in-memory snapshot. Claiming the whole file at startup (which is what `crate::provenance` does,
/// and what this used to copy) makes that a real loss path: launch, let another window add a note
/// elsewhere, then save anything at all and the other window's note is overwritten by a
/// launch-time copy. Ownership is now taken per worktree, only where this window really wrote.
#[gpui::test]
fn saving_does_not_overwrite_a_worktree_this_window_only_read(cx: &mut TestAppContext) {
    let (repo, store) = repo_with_an_agent_authored_change(1_700_000_000);
    let shim_dir = TempDir::new().expect("tempdir");
    let state_dir = TempDir::new().expect("tempdir");
    let notes_file =
        super::persist_state::review_notes_path_for(&state_dir.path().join("settings.toml"));

    // Another window's worktree, already on disk before this one launches.
    let other = PathBuf::from("/repo/another-window");
    let mut theirs = super::NoteStore::default();
    theirs.set_text(&other, Path::new(PATH), NoteAnchor::New(1), "theirs");
    super::persist_state::ReviewNotesState::capture(&theirs)
        .save_at(&notes_file)
        .expect("seed the other window's notes");

    let (app, cx, _agent) = open_review(cx, &repo, &shim_dir, store, 1_700_000_000);
    app.update(cx, |app, _| {
        app.review_notes_path = Some(notes_file.clone());
        // Exactly what a launch does: read the whole file into memory.
        app.restore_review_notes();
    });
    cx.run_until_parked();

    // Now write in *this* window's own worktree, and let the save land.
    click_line(cx, 2);
    cx.simulate_input("ours");
    cx.run_until_parked();
    cx.background_executor
        .advance_clock(Duration::from_millis(700));
    cx.run_until_parked();

    let mut merged = super::NoteStore::default();
    super::persist_state::ReviewNotesState::load_at(&notes_file).restore_into(&mut merged);
    assert_eq!(
        merged
            .note(&other, Path::new(PATH), NoteAnchor::New(1))
            .map(|note| note.text.as_str()),
        Some("theirs"),
        "a worktree this window only read must come back untouched"
    );
    let worktree = app.read_with(cx, |app, _| app.review_notes_worktree());
    assert_eq!(
        merged.file_state(&worktree, Path::new(PATH)).count,
        1,
        "and this window's own note must really have been written"
    );
}
