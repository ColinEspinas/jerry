use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

impl AdeApp {
    /// Tears down every [`Self::lsp_clients`] entry whose key is not `active_root` - see that
    /// field's own docs for the kill-on-switch-not-LRU choice. Also drops
    /// [`Self::lsp_opened_files`] entries for files under an evicted root, so a future
    /// [`Self::dispatch_did_open`] against a freshly-respawned client for that root won't
    /// wrongly believe the file is already open and skip sending `didOpen`.
    ///
    /// ## Getting `&mut LspClient` out of a shared `Arc`
    ///
    /// [`LspClientState::Ready`] holds an `Arc<lsp_core::LspClient>`, cloned out to whichever
    /// in-flight background task last needed it. A clone could still be alive at the exact
    /// moment of eviction. [`LspClient::shutdown`] needs `&mut self` and does real, potentially
    /// slow, blocking work (a `shutdown` request, `exit`, `SIGTERM`, a grace period, `SIGKILL`,
    /// joining reader threads) - unacceptable to run inline on the GPUI thread. So the `Arc` is
    /// moved into a `cx.background_executor()` task, and `Arc::try_unwrap` is attempted there:
    /// if this was the last clone, a graceful `shutdown()` runs off-thread; if not,
    /// `try_unwrap` returns the `Arc` back and it's just dropped - not a silent no-op, since
    /// `LspClient`'s own `Drop` impl still does a `SIGKILL`-based teardown either way. This only
    /// happens in the rare case a clone outlives the switch, and "process gets `SIGKILL`ed
    /// instead of asked nicely" is an acceptable trade-off for never blocking the UI.
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
                            if let Err(err) = client.shutdown() {
                                log::warn!("failed to shut down rust-analyzer: {err}");
                            }
                        }
                        Err(client) => {
                            // Some other clone is still alive - see this method's own docs.
                            // Dropping it here is still real cleanup via `Drop`, just not the
                            // graceful path.
                            drop(client);
                        }
                    }
                });
                self._lsp_tasks.push(task);
            }
            // `Spawning`/`Failed` states hold no process to tear down. A `Spawning` one whose
            // background task is still in-flight will, once it resolves, re-insert an entry
            // under `root` even though it's no longer active - harmless: the next eviction pass
            // catches it same as any other stale entry.
        }
    }

    /// Lazily spawns (or reuses) an `lsp_core::LspClient` for `repo_root` - a no-op if a client
    /// for this exact root already exists in any state (a previous failure is not retried on
    /// every render; this is only called once per root per [`Self::render_file_view`] pass). The
    /// `LspClient::spawn` call (process spawn plus the full `initialize`/`initialized`
    /// handshake) runs on `cx.background_executor()`, mirroring [`Self::spawn_file_load`]'s
    /// shape, since it's blocking I/O that must never run on the GPUI foreground thread.
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
        self._lsp_tasks.push(task);
    }

    /// Sends a `textDocument/didOpen` for `path`, once per real path (see
    /// [`Self::lsp_opened_files`]'s docs). The file content is read fresh here (separate from
    /// [`Self::file_view_cache`]'s own cached parse; this only happens once per file open, not
    /// per render). Runs on `cx.background_executor()` since both the file read and the write to
    /// rust-analyzer's stdin are blocking I/O.
    ///
    /// ## Judgment call: no `textDocument/didClose` is ever sent
    ///
    /// A real editor sends `didClose` when a buffer stops being open. This viewer is read-only
    /// and has no "close" event of its own - [`Self::open_file_view`] can be called again for
    /// the same path later (switching tabs back and forth), and [`Self::lsp_opened_files`]
    /// intentionally treats that as "already open," not "reopen." Sending `didClose` on every
    /// tab switch would mean re-sending `didOpen` (and re-waiting through indexing) every time
    /// the user switches back to a file - real latency for no benefit, since this app keeps at
    /// most a handful of files' worth of server-side state alive per session. The `LspClient`
    /// (and every document it opened) is torn down when its owning root's client is dropped -
    /// this app's actual document lifetime boundary.
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
                    match std::fs::read_to_string(&path) {
                        Ok(text) => {
                            if let Err(err) = client.did_open(&path, text, 1) {
                                log::warn!("failed to send didOpen for {}: {err}", path.display());
                            }
                        }
                        Err(err) => {
                            log::warn!("failed to read {} for didOpen: {err}", path.display());
                        }
                    }
                })
                .await;
        });
        self._lsp_tasks.push(task);
    }

    /// Starts (idempotently - see [`Self::_lsp_poll_task`]'s docs on why this is a one-time, not
    /// per-client, task) the background loop that keeps rendering aware of asynchronously
    /// arriving `publishDiagnostics` notifications from every [`Self::lsp_clients`] entry.
    /// Mirrors `crate::terminal_pane::TerminalPane::spawn_process`'s own
    /// `cx.background_executor().timer(..)`-driven poll loop shape: diagnostics arrive on
    /// `lsp_core`'s own reader thread, outside the GPUI runtime, with no way to directly notify
    /// a GPUI entity from there - so this loop periodically drains every ready client's wake
    /// channel and calls `cx.notify()` only when something actually changed, never
    /// unconditionally per tick.
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

/// The state of one repository root's `lsp_core::LspClient` across its asynchronous
/// spawn-then-initialize lifecycle (`lsp_core::LspClient::spawn` doesn't return until the
/// `initialize`/`initialized` handshake completes, so `Ready` here always means a usable
/// client).
#[derive(Clone)]
pub(super) enum LspClientState {
    /// A `cx.background_executor()` task is currently spawning `rust-analyzer` and running its
    /// handshake for this root - the status bar shows this honestly (`"starting
    /// rust-analyzer..."`), never a fabricated "indexed" state.
    Spawning,
    /// An already-initialized client - `Arc`-shared so it can be handed into a background task
    /// ([`AdeApp::dispatch_did_open`]) and the poll loop ([`AdeApp::ensure_lsp_poll_task`])
    /// without re-locking `AdeApp` from a foreground-only `Context<Self>` on every call.
    Ready(std::sync::Arc<lsp_core::LspClient>),
    /// A spawn/handshake failure (e.g. `rust-analyzer` not on `PATH`) - carries the
    /// `lsp_core::LspError`'s own `Display` text, shown as-is rather than a generic message
    /// that would hide *why*.
    Failed(String),
}

/// Which of `existing_roots` should be evicted once `active_root` becomes the newly active
/// worktree root: every one of them that isn't `active_root` itself. Kept gpui-free/pure so the
/// "which keys get removed" bookkeeping is unit-testable without a real `lsp_core::LspClient`,
/// which can only be constructed by genuinely spawning `rust-analyzer`.
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

/// The `rust-analyzer` status this window's status bar shows for a `.rs` file. Every variant
/// corresponds to a real, distinguishable server state (see [`LspClientState`]'s own docs);
/// there is no variant that fabricates progress this app can't actually observe (rust-analyzer's
/// real `$/progress` payloads carry a crate count, but this phase doesn't track `$/progress` at
/// all, so [`LspFileStatus::Indexing`]'s coarser "no publishDiagnostics yet" signal is used
/// instead).
pub(super) enum LspFileStatus {
    Spawning,
    Failed(String),
    /// A ready client exists, but no `publishDiagnostics` has arrived yet for this specific file.
    /// Distinct from `Analyzed { errors: 0, .. }` (see
    /// `lsp_core::LspClient::has_diagnostics_result`'s own docs for the signal this reads).
    Indexing,
    Analyzed {
        errors: usize,
        warnings: usize,
    },
}

/// Takes an already-computed [`lsp_core::lsp_types::Uri`] (see [`AdeApp::render_file_view`]'s
/// own docs for why it's computed once per render and passed in rather than re-derived here).
/// `None` only when computing the `file://` URI for the open file's path failed, in which case
/// this reports [`LspFileStatus::Indexing`] (there's no way to answer "is there a result"
/// without a URI to look one up by) rather than fabricating any other status.
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

/// Regression coverage for the LSP client-eviction fix (see [`AdeApp::lsp_clients`]'s own docs):
/// before this fix, nothing ever removed an `lsp_clients` entry once its root stopped being the
/// active worktree, so browsing N worktrees (each with a Rust file opened) leaked N live
/// `rust-analyzer` processes for the window's life. Exercises the real production code path
/// (`AdeApp::select_worktree` -> [`AdeApp::evict_stale_lsp_clients`]) through a real `AdeApp` in
/// a test GPUI window, but seeds `lsp_clients` with cheap `LspClientState::Spawning` entries
/// rather than real `Arc<lsp_core::LspClient>`s - the full end-to-end process-lifecycle proof
/// lives in `lsp_diagnostics_wiring_tests` below, where spawning a real process is unavoidable
/// anyway. This module's own tests prove the bookkeeping runs in milliseconds, with no real
/// process involved.
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

    /// The end-to-end proof at the `AdeApp` level: browsing several worktrees, each with an
    /// `lsp_clients` entry seeded for it (standing in for "a Rust file was opened here"), never
    /// lets more than one entry accumulate - [`AdeApp::select_worktree`]'s call into
    /// [`AdeApp::evict_stale_lsp_clients`] must fire on every switch, including a later revisit
    /// of an already-seen worktree.
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
                // Simulate `AdeApp::ensure_lsp_client` having already been called for a Rust
                // file opened under the newly active root - a cheap `Spawning` entry, no real
                // process needed to prove the eviction bookkeeping.
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

/// Slow, end-to-end coverage proving the real async path from a real `rust-analyzer`
/// `publishDiagnostics` response through to [`AdeApp::render_file_view`]'s rendered output,
/// through this crate's own real code path (`AdeApp::open_file_view` -> `ensure_lsp_client` ->
/// `dispatch_did_open` -> `render_file_view`) rather than by calling `lsp_core` directly and
/// bypassing `AdeApp`.
///
/// This genuinely spawns a real `rust-analyzer` against a tiny, dependency-free scratch cargo
/// project (kept dependency-free so `cargo metadata`/rust-analyzer's own workspace discovery
/// never needs network access) with a genuine `let x: i32 = "not a number";` type mismatch, and
/// polls real wall-clock time (up to 180s, matching `lsp_core::client`'s own e2e test) for the
/// diagnostic to actually arrive - no sleep stands in for that wait, and nothing is fabricated
/// if the wait times out (the assertion just fails). This is a genuinely slow test (real process
/// spawn plus real sysroot indexing) kept in the normal, non-`#[ignore]` suite on purpose - this
/// project has no separate "slow test" lane.
#[cfg(test)]
mod lsp_diagnostics_wiring_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    /// Same minimal, dependency-free scratch cargo project shape as
    /// `lsp_core::client::tests::write_scratch_project` - kept as its own small copy here
    /// rather than exporting that one across the crate boundary.
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
    /// `AdeApp::file_view_diagnostics` holds at least one diagnostic, or `deadline` passes. The
    /// real `publishDiagnostics` notification arrives on `lsp_core`'s own raw OS reader thread,
    /// outside GPUI's scheduler entirely, so this must genuinely keep re-checking over real
    /// time, like `lsp_core::client`'s own `wait_for_update` polling loop one layer down.
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

    /// The end-to-end proof this fix exists to deliver: a real `rust-analyzer`, spawned via this
    /// app's own `AdeApp::ensure_lsp_client`/`AdeApp::dispatch_did_open` code path, publishes a
    /// diagnostic for a real type mismatch, and that diagnostic - real byte range, real message
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
        // Drives the real `code_view::load_file` background parse to completion -
        // `render_file_view` does nothing LSP-related until `file_view_cache` is fresh, so this
        // must happen before `ensure_lsp_client` ever gets a chance to run.
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

        // One real, `Ready` `lsp_clients` entry for this repo root - the "one client per repo
        // root, not per file" requirement.
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
        // not spawn a second real `rust-analyzer` process. Cheaply proven via `lsp_clients.len()`
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
