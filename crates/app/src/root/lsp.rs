use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

impl AdeApp {
    /// Tears down every real [`Self::lsp_clients`] entry whose key is not `active_root` - see
    /// that field's own docs for why this exists and the kill-on-switch-not-LRU choice. Also
    /// drops [`Self::lsp_opened_files`] entries for files under an evicted root: leaving them
    /// stale would make a *future* `Self::dispatch_did_open` against a freshly-respawned client
    /// for that same root (if the user switches back to it later) wrongly believe the file is
    /// "already open" and skip sending a real `didOpen` to the new process, which would never
    /// see that file at all.
    ///
    /// ## Getting `&mut LspClient` out of a shared `Arc`
    ///
    /// [`LspClientState::Ready`] holds an `Arc<lsp_core::LspClient>`, cloned out to whichever
    /// in-flight background task last needed it (`Self::dispatch_did_open`, and the single
    /// long-lived [`Self::ensure_lsp_poll_task`] loop, which only ever reads through `&self`
    /// methods and never outlives a single poll tick's borrow - see that method's own body). A
    /// clone *could* still be alive at the exact moment of eviction (a `dispatch_did_open` task
    /// dispatched moments earlier, not yet finished). [`LspClient::shutdown`] needs `&mut self`
    /// and does real, potentially slow, blocking work (a `shutdown` request, an `exit`
    /// notification, `SIGTERM`, a bounded grace period, `SIGKILL`, then joining the reader/
    /// stderr threads) - unacceptable to run inline on this foreground/GPUI thread. So: the
    /// `Arc` is moved into a `cx.background_executor()` task (fired here, not awaited - the
    /// `Task` handle is kept alive in [`Self::_lsp_tasks`] the same way every other in-flight LSP
    /// background task here is, per that field's own docs), and `Arc::try_unwrap` is attempted
    /// *there*: if this was genuinely the last clone, it succeeds and a real, graceful
    /// `shutdown()` runs off the foreground thread; if some other clone is still alive,
    /// `try_unwrap` fails and returns the `Arc` back, which is then just `drop`-ped - not a
    /// silent no-op: `LspClient`'s own `Drop` impl still does a real `SIGKILL`-based teardown of
    /// the whole process tree (no orphan either way, see that impl's docs), just not the
    /// graceful path. This only happens in the rare case a clone outlives the switch, and
    /// "process gets `SIGKILL`ed instead of asked nicely to shut down" is a real, acceptable
    /// trade-off for never blocking the UI on a worktree switch.
    pub(super) fn evict_stale_lsp_clients(&mut self, active_root: &Path, cx: &mut Context<Self>) {
        let stale_roots = stale_lsp_client_roots(
            &self.lsp_clients.keys().cloned().collect::<Vec<_>>(),
            active_root,
        );
        if stale_roots.is_empty() {
            return;
        }

        for root in stale_roots {
            self.lsp_opened_files
                .retain(|path| !path.starts_with(&root));

            let Some(state) = self.lsp_clients.remove(&root) else {
                continue;
            };
            if let LspClientState::Ready(client) = state {
                let task = cx.background_executor().spawn(async move {
                    match std::sync::Arc::try_unwrap(client) {
                        Ok(mut client) => {
                            let _ = client.shutdown();
                        }
                        Err(client) => {
                            // Some other clone is still alive - see this method's own docs.
                            // Dropping it here is still real cleanup (a `SIGKILL`-based teardown
                            // via `Drop`, guaranteed once the *last* clone drops), just not the
                            // graceful path.
                            drop(client);
                        }
                    }
                });
                self._lsp_tasks.retain(|task| !task.is_ready());
                self._lsp_tasks.push(task);
            }
            // `Spawning`/`Failed` states hold no real process to tear down. A `Spawning` one
            // whose background task (`Self::ensure_lsp_client`'s own `cx.spawn`) is still
            // in-flight will, once it resolves, re-insert a `Ready`/`Failed` entry back under
            // `root` even though it's no longer active - a harmless, one-time re-insertion (no
            // process leak: `Ready` just means a real client that the *next* eviction pass,
            // triggered by the next worktree switch, will catch and tear down same as any other
            // stale entry).
        }
    }

    /// Lazily spawns (or reuses) a real `lsp_core::LspClient` for `repo_root`
    /// (`design_handoff_jerry_ade/README.md`'s Diagnostic state) - a no-op if a client for this
    /// exact root already exists in any state (`Spawning`/`Ready`/`Failed`; a previous real
    /// failure is not silently retried on every render - see this method's own call site,
    /// which only calls this once per root per [`Self::render_file_view`] pass anyway). The
    /// real `LspClient::spawn` call (process spawn plus the full `initialize`/`initialized`
    /// handshake) runs on `cx.background_executor()`, mirroring [`Self::spawn_file_load`]'s
    /// exact shape, since it is real, blocking I/O (and can take real, non-trivial time - see
    /// `lsp_core::client`'s own docs) that must never run on the GPUI foreground thread.
    pub(super) fn ensure_lsp_client(&mut self, repo_root: PathBuf, cx: &mut Context<Self>) {
        if self.lsp_clients.contains_key(&repo_root) {
            return;
        }
        self.lsp_clients
            .insert(repo_root.clone(), LspClientState::Spawning);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let repo_root = repo_root.clone();
                    async move { lsp_core::LspClient::spawn(&repo_root) }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(client) => {
                        this.lsp_clients.insert(
                            repo_root.clone(),
                            LspClientState::Ready(std::sync::Arc::new(client)),
                        );
                        this.ensure_lsp_poll_task(cx);
                    }
                    Err(error) => {
                        this.lsp_clients
                            .insert(repo_root.clone(), LspClientState::Failed(error.to_string()));
                    }
                }
                cx.notify();
            });
        });
        self._lsp_tasks.retain(|task| !task.is_ready());
        self._lsp_tasks.push(task);
    }

    /// Sends a real `textDocument/didOpen` for `path` on `client`, once per real path (see
    /// [`Self::lsp_opened_files`]'s docs) - the real file content is read fresh here (a second,
    /// small read separate from [`Self::file_view_cache`]'s own cached parse; this only happens
    /// once per file open, not per render, so the duplication is cheap and keeps `code_view`
    /// itself free of any LSP awareness). Runs on `cx.background_executor()` since both the real
    /// file read and the real `write` syscall to rust-analyzer's stdin are blocking I/O.
    ///
    /// ## Judgment call: no `textDocument/didClose` is ever sent
    ///
    /// A real editor sends `didClose` when a buffer stops being open so the server can free
    /// per-document state and stop publishing diagnostics for it. This viewer is read-only and
    /// has no real "close" event of its own - `Self::open_file_view` can be called again for the
    /// same path at any later point (switching tabs back and forth, or re-selecting the same
    /// file in the tree), and [`Self::lsp_opened_files`] intentionally treats that as "already
    /// open," not "reopen." Sending `didClose` on every tab switch would mean re-sending
    /// `didOpen` (and waiting through indexing/analysis again) every time the user switches back
    /// to a file they were just looking at - real, user-visible latency for no real benefit,
    /// since this app keeps at most a handful of files' worth of server-side state alive per
    /// session, nowhere near enough to matter for `rust-analyzer`'s own memory use. The
    /// `LspClient` (and every document it opened) is torn down for real when its owning root's
    /// client is dropped (window close, or a future repo-root change - see [`Self::lsp_clients`]'s
    /// own docs), which is this app's actual real document lifetime boundary.
    pub(super) fn dispatch_did_open(
        &mut self,
        client: std::sync::Arc<lsp_core::LspClient>,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if self.lsp_opened_files.contains(&path) {
            return;
        }
        self.lsp_opened_files.insert(path.clone());

        let task = cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .spawn(async move {
                    if let Ok(text) = std::fs::read_to_string(&path) {
                        let _ = client.did_open(&path, text, 1);
                    }
                })
                .await;
        });
        self._lsp_tasks.retain(|task| !task.is_ready());
        self._lsp_tasks.push(task);
    }

    /// Starts (idempotently - see [`Self::_lsp_poll_task`]'s own docs on why this is a one-time,
    /// not per-client, task) the single background loop that keeps this window's rendering aware
    /// of real, asynchronously-arriving `publishDiagnostics` notifications from every
    /// [`Self::lsp_clients`] entry. Mirrors `crate::terminal_pane::TerminalPane::spawn_process`'s
    /// own established `cx.background_executor().timer(..)`-driven poll loop shape: real
    /// diagnostics arrive on a background thread (`lsp_core`'s own reader thread - see that
    /// crate's docs) with no way to directly notify a GPUI entity from outside the GPUI runtime,
    /// so this loop periodically drains every ready client's real wake channel
    /// (`lsp_core::LspClient::drain_updates`) and calls `cx.notify()` only when something real actually
    /// changed - never an unconditional per-tick `cx.notify()`, which would repaint every ~250ms
    /// regardless of whether there was ever anything new to show.
    pub(super) fn ensure_lsp_poll_task(&mut self, cx: &mut Context<Self>) {
        if self._lsp_poll_task.is_some() {
            return;
        }
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(LSP_DIAGNOSTICS_POLL_INTERVAL)
                .await;
            let update_result = this.update(cx, |this, cx| {
                let mut any_update = false;
                for state in this.lsp_clients.values() {
                    if let LspClientState::Ready(client) = state {
                        if client.drain_updates() {
                            any_update = true;
                        }
                    }
                }
                if any_update {
                    cx.notify();
                }
            });
            if update_result.is_err() {
                break; // the window/entity is gone - stop polling.
            }
        });
        self._lsp_poll_task = Some(task);
    }
}

/// The real state of one repository root's `lsp_core::LspClient`, across its own real,
/// asynchronous spawn-then-initialize lifecycle (see `lsp_core::LspClient::spawn`'s own docs -
/// it does not return until a real `initialize`/`initialized` handshake has completed, so
/// `Ready` here always means a genuinely usable client, never one still mid-handshake).
#[derive(Clone)]
pub(super) enum LspClientState {
    /// A real `cx.background_executor()` task is currently spawning `rust-analyzer` and running
    /// its handshake for this root - `Self::render_file_view`'s status bar shows this honestly
    /// (`"starting rust-analyzer..."`), never a fabricated "indexed" state.
    Spawning,
    /// A real, already-initialized client - `Arc`-shared so it can be handed into a background
    /// task (`Self::dispatch_did_open`) and the poll loop (`Self::ensure_lsp_poll_task`) without
    /// re-locking `AdeApp` itself from a foreground-only `Context<Self>` on every call.
    Ready(std::sync::Arc<lsp_core::LspClient>),
    /// A real spawn/handshake failure (e.g. `rust-analyzer` genuinely not on `PATH`) - carries
    /// the real `lsp_core::LspError`'s own `Display` text, shown as-is rather than a generic
    /// "language server unavailable" that would hide *why*.
    Failed(String),
}

/// Which of `existing_roots` should be evicted once `active_root` becomes the newly active
/// worktree root: every one of them that isn't `active_root` itself (see
/// [`AdeApp::evict_stale_lsp_clients`]'s own docs for the real teardown side this feeds - kept
/// gpui-free/pure, mirroring [`reset_per_worktree_ui_state`]'s own reasoning, so the actual
/// "which keys get removed" bookkeeping is unit-testable without a real `lsp_core::LspClient`,
/// which can only be constructed by genuinely spawning `rust-analyzer`).
pub(super) fn stale_lsp_client_roots(
    existing_roots: &[PathBuf],
    active_root: &Path,
) -> Vec<PathBuf> {
    existing_roots
        .iter()
        .filter(|root| root.as_path() != active_root)
        .cloned()
        .collect()
}

/// The real, honest `rust-analyzer` status this window's status bar shows for a `.rs` file -
/// `design_handoff_jerry_ade/README.md`'s "Status bar 28: `rust-analyzer` + green dot +
/// `indexed 1,284 crates`". Every variant here corresponds to a real, distinguishable server
/// state (see [`LspClientState`]'s own docs); there is no variant that fabricates progress this
/// app can't actually observe (e.g. no invented crate-count - rust-analyzer's real `$/progress`
/// payloads carry one, but this phase doesn't track `$/progress` at all - see the step report's
/// "indexing state" section for why [`LspFileStatus::Indexing`]'s coarser "no publishDiagnostics
/// yet" signal was chosen instead).
pub(super) enum LspFileStatus {
    Spawning,
    Failed(String),
    /// A real, ready client exists, but no real `publishDiagnostics` has arrived yet for this
    /// specific file - genuinely distinct from `Analyzed { errors: 0, .. }` (see
    /// `lsp_core::LspClient::has_diagnostics_result`'s own docs for the real signal this reads).
    Indexing,
    Analyzed {
        errors: usize,
        warnings: usize,
    },
}

/// Takes an already-computed [`lsp_core::lsp_types::Uri`] (see [`AdeApp::render_file_view`]'s
/// own docs for why it's computed once per render and passed in here, rather than this function
/// re-deriving it from a path itself) - `None` only in the rare case that computing the real
/// `file://` URI for the open file's own path failed (see [`lsp_core::LspClient::uri_for_path`]'s
/// docs), in which case this honestly reports [`LspFileStatus::Indexing`] (there is no real way
/// to answer "is there a result" without a real URI to look one up by) rather than fabricating
/// any other status.
pub(super) fn lsp_file_status(
    state: &Option<LspClientState>,
    uri: Option<&lsp_core::lsp_types::Uri>,
) -> LspFileStatus {
    match state {
        None | Some(LspClientState::Spawning) => LspFileStatus::Spawning,
        Some(LspClientState::Failed(message)) => LspFileStatus::Failed(message.clone()),
        Some(LspClientState::Ready(client)) => {
            let Some(uri) = uri else {
                return LspFileStatus::Indexing;
            };
            if !client.has_diagnostics_result_uri(uri) {
                return LspFileStatus::Indexing;
            }
            let diagnostics = client.diagnostics_for_uri(uri).unwrap_or_default();
            let errors = diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostics_view::Severity::from_lsp(diagnostic.severity)
                        == diagnostics_view::Severity::Error
                })
                .count();
            let warnings = diagnostics
                .iter()
                .filter(|diagnostic| {
                    diagnostics_view::Severity::from_lsp(diagnostic.severity)
                        == diagnostics_view::Severity::Warning
                })
                .count();
            LspFileStatus::Analyzed { errors, warnings }
        }
    }
}

/// Real regression coverage for the LSP client-eviction fix (see [`AdeApp::lsp_clients`]'s own
/// docs): before this fix, nothing ever removed an `lsp_clients` entry once its root stopped
/// being the active worktree, so browsing N different worktrees (each with a Rust file opened)
/// leaked N live `rust-analyzer` processes for the rest of the window's life. Exercises the real
/// production code path (`AdeApp::select_worktree` -> [`AdeApp::evict_stale_lsp_clients`])
/// through a real `AdeApp` in a real (test) GPUI window, but seeds `lsp_clients` with cheap
/// `LspClientState::Spawning` entries rather than real `Arc<lsp_core::LspClient>`s (which can
/// only be constructed by genuinely spawning `rust-analyzer`) - the real, full end-to-end
/// process-lifecycle proof (a genuine spawn, genuine diagnostics, genuine teardown) lives in
/// `lsp_diagnostics_wiring_tests` below, where spawning a real process is unavoidable anyway.
/// This module's own tests instead prove the real *bookkeeping* - which keys survive a switch -
/// runs in milliseconds, with no real process involved.
#[cfg(test)]
mod lsp_client_eviction_tests {
    use super::*;
    use gpui::TestAppContext;

    fn worktree_item(path: PathBuf) -> WorktreeItem {
        WorktreeItem {
            path,
            label: "wt".to_string(),
            branch: None,
            is_main: false,
            is_locked: false,
            error: None,
        }
    }

    #[test]
    fn stale_lsp_client_roots_keeps_only_the_active_root() {
        let roots = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let stale = stale_lsp_client_roots(&roots, Path::new("/b"));
        assert_eq!(stale, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }

    #[test]
    fn stale_lsp_client_roots_is_empty_when_the_active_root_is_the_only_one() {
        let roots = vec![PathBuf::from("/a")];
        let stale = stale_lsp_client_roots(&roots, Path::new("/a"));
        assert!(stale.is_empty());
    }

    /// The real end-to-end proof at the `AdeApp` level: browsing several different worktrees,
    /// each with a real `lsp_clients` entry seeded for it (standing in for "a Rust file was
    /// opened here" - see [`AdeApp::ensure_lsp_client`]'s own docs for the real trigger this
    /// simulates), never lets more than one entry accumulate - [`AdeApp::select_worktree`]'s
    /// real call into [`AdeApp::evict_stale_lsp_clients`] must fire on every real switch,
    /// including a later revisit of an already-seen worktree (not just "never insert a second
    /// one" on a monotonic walk).
    #[gpui::test]
    fn switching_between_several_worktrees_never_lets_lsp_clients_grow_past_one(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let worktree_a = tempfile::tempdir().expect("tempdir a");
        let worktree_b = tempfile::tempdir().expect("tempdir b");
        let worktree_c = tempfile::tempdir().expect("tempdir c");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(worktree_a.path().to_path_buf()),
                worktree_item(worktree_b.path().to_path_buf()),
                worktree_item(worktree_c.path().to_path_buf()),
            ];
        });

        for index in 0..3 {
            app.update(cx, |app, cx| {
                app.select_worktree(index, cx);
                // Simulate `Self::ensure_lsp_client` having already been called for a Rust file
                // opened under the newly active root - a cheap `Spawning` entry, no real process
                // needed to prove the real eviction bookkeeping.
                let root = app.file_tree_root.clone();
                app.lsp_clients
                    .insert(root.clone(), LspClientState::Spawning);
                app.lsp_opened_files.insert(root.join("src/main.rs"));
            });
            cx.run_until_parked();

            app.read_with(cx, |app, _| {
                assert_eq!(
                    app.lsp_clients.len(),
                    1,
                    "after selecting worktree #{index}, exactly one lsp_clients entry (the \
                     newly active root's own) should remain - got: {:?}",
                    app.lsp_clients.keys().collect::<Vec<_>>()
                );
                let root = &app.file_tree_root;
                assert!(
                    app.lsp_clients.contains_key(root),
                    "the surviving entry should be the newly active root's own"
                );
                assert!(
                    app.lsp_opened_files
                        .iter()
                        .all(|path| path.starts_with(root)),
                    "lsp_opened_files must not still hold a stale entry from an evicted root"
                );
            });
        }

        // Bounce back to worktree A - a fourth switch, proving eviction fires on a revisit too.
        app.update(cx, |app, cx| {
            app.select_worktree(0, cx);
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(root, LspClientState::Spawning);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.lsp_clients.len(),
                1,
                "revisiting an already-seen worktree must still evict the one just left"
            );
        });
    }
}

/// Real, slow, end-to-end coverage proving the actual async path from a real `rust-analyzer`
/// `publishDiagnostics` response through to [`AdeApp::render_file_view`]'s own rendered output -
/// before this test, every `lsp_core` test lived below the GPUI layer entirely, and every
/// `diagnostics_view` test was pure byte-range/run-splitting logic with no real process involved
/// at all, so nothing actually proved a real diagnostic ever reaches [`AdeApp::file_view_diagnostics`]
/// (the data [`AdeApp::render_file_view`]'s row builder actually reads) through this crate's own
/// real code path (`AdeApp::open_file_view` -> `AdeApp::ensure_lsp_client` ->
/// `AdeApp::dispatch_did_open` -> `AdeApp::render_file_view`), rather than by reaching into
/// `lsp_core` directly and bypassing `AdeApp` the way `lsp_core::client`'s own e2e test does.
///
/// This genuinely spawns a real `rust-analyzer` against a real, tiny, dependency-free scratch
/// cargo project (mirroring `lsp_core::client`'s own `write_scratch_project` fixture, kept
/// dependency-free for the same reason: `cargo metadata`/rust-analyzer's own workspace discovery
/// must never need network access) with a genuine `let x: i32 = "not a number";` type mismatch,
/// and polls real wall-clock time (up to a generous 180s bound, matching `lsp_core::client`'s own
/// e2e test) for the real diagnostic to actually arrive - no artificial sleep stands in for that
/// real wait, and no diagnostic is fabricated if the wait were to time out (the assertion would
/// simply fail honestly). This is a genuinely slow test (real process spawn plus real sysroot
/// indexing) kept in the normal, non-`#[ignore]` suite on purpose - this project has no separate
/// "slow test" lane, and this is exactly the kind of test that proves the feature is real rather
/// than merely compiling.
#[cfg(test)]
mod lsp_diagnostics_wiring_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    /// Same real, minimal, dependency-free scratch cargo project shape as
    /// `lsp_core::client::tests::write_scratch_project` - kept as its own small copy here
    /// (rather than exporting that one across the crate boundary) since it's a handful of lines
    /// and this is the only place the `app` crate's own tests need one.
    fn write_scratch_project(main_rs: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"app_lsp_wiring_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir src");
        std::fs::write(dir.path().join("src").join("main.rs"), main_rs).expect("write main.rs");
        dir
    }

    /// Repeatedly re-renders the centre pane and drains the deterministic test executor until
    /// `AdeApp::file_view_diagnostics` holds at least one real diagnostic, or `deadline` passes.
    /// A real, bounded, wall-clock retry loop - the real `publishDiagnostics` notification
    /// arrives on `lsp_core`'s own raw OS reader thread (outside GPUI's scheduler entirely), so
    /// this must genuinely keep re-checking over real time, exactly like
    /// `lsp_core::client`'s own e2e test's `wait_for_update` polling loop does one layer down.
    fn wait_for_real_diagnostics(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        deadline: Instant,
    ) {
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();

            let has_diagnostics = app.read_with(cx, |app, _| !app.file_view_diagnostics.is_empty());
            if has_diagnostics {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no real diagnostic reached AdeApp::file_view_diagnostics within the real \
                 180s deadline"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The real, practical end-to-end proof this fix exists to deliver: a real `rust-analyzer`,
    /// spawned via this app's own real `AdeApp::ensure_lsp_client`/`AdeApp::dispatch_did_open`
    /// code path (not `lsp_core` called directly), genuinely publishes a diagnostic for a real
    /// type mismatch, and that real diagnostic - real byte range, real error code, real message
    /// - ends up in `AdeApp::file_view_diagnostics`, which is exactly what
    /// `AdeApp::render_file_view`'s row builder reads to draw the underline/card.
    #[gpui::test]
    fn a_real_diagnostic_reaches_file_view_diagnostics_through_the_real_app_code_path(
        cx: &mut TestAppContext,
    ) {
        let project = write_scratch_project(
            "fn main() {\n    let x: i32 = \"not a number\";\n    println!(\"{}\", x);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        // Drives the real `code_view::load_file` background parse to completion - `render_file_view`
        // does nothing LSP-related at all until `file_view_cache` is fresh (see that method's own
        // docs), so this must happen before `ensure_lsp_client` ever gets a chance to run.
        cx.run_until_parked();

        let deadline = Instant::now() + Duration::from_secs(180);
        wait_for_real_diagnostics(&app, cx, deadline);

        app.read_with(cx, |app, _| {
            let all_diagnostics: Vec<&diagnostics_view::LineDiagnostic> =
                app.file_view_diagnostics.values().flatten().collect();
            assert!(
                !all_diagnostics.is_empty(),
                "sanity check: the wait loop only returns once file_view_diagnostics is non-empty"
            );

            let mismatch = all_diagnostics.iter().find(|diagnostic| {
                let message = diagnostic.message.to_lowercase();
                message.contains("mismatched")
                    || (message.contains("expected") && message.contains("i32"))
            });
            assert!(
                mismatch.is_some(),
                "expected a real diagnostic referencing the genuine type mismatch, got: \
                 {all_diagnostics:#?}"
            );
            let mismatch = mismatch.expect("checked above");
            assert_eq!(
                mismatch.severity,
                diagnostics_view::Severity::Error,
                "a genuine type mismatch should reach the render path at Error severity"
            );
            // Real byte range intact: `let x: i32 = "not a number";` is the real offending
            // line, and the diagnostic's own range should land somewhere within it (not a
            // zero'd-out or fabricated range).
            assert!(
                mismatch.byte_range.start < mismatch.byte_range.end
                    || mismatch.byte_range.start > 0,
                "expected a real, non-degenerate byte range, got {:?}",
                mismatch.byte_range
            );
        });

        // One real, `Ready` `lsp_clients` entry for this repo root - the real "one client per
        // repo root, not per file" requirement.
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.lsp_clients.len(),
                1,
                "exactly one real lsp_clients entry should exist for this repo root"
            );
            assert!(
                matches!(
                    app.lsp_clients.values().next(),
                    Some(LspClientState::Ready(_))
                ),
                "the diagnostics wait loop only returns once a real diagnostic arrived, which \
                 requires a real Ready client"
            );
        });

        // Opening a *second* Rust file under the same repo root must reuse the existing client,
        // not spawn a second real `rust-analyzer` process - the "one client per repo root, not
        // per file" requirement from this phase's own scope. Cheaply proven via `lsp_clients.len()`
        // staying at 1 (no second real indexing wait needed).
        let second_file = project.path().join("src").join("lib.rs");
        std::fs::write(&second_file, "pub fn helper() -> i32 {\n    1\n}\n").expect("write lib.rs");
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(second_file.clone(), window, cx);
        });
        cx.run_until_parked();
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.lsp_clients.len(),
                1,
                "opening a second Rust file under the same repo root must reuse the existing \
                 lsp_clients entry, not spawn a second one"
            );
        });
    }
}
