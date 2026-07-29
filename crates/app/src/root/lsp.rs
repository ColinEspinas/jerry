use super::*;
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

/// [`AdeApp::lsp_clients`]' real key: a repository root paired with the real server binary
/// running for it - see that field's own docs for why a bare `PathBuf` was widened to this
/// (Revision R8) once more than one language could have a live client under the same root.
pub(super) type LspClientKey = (PathBuf, &'static str);

impl AdeApp {
    /// Tears down every [`Self::lsp_clients`] entry whose *root* is not `active_root` - every
    /// language's client for the old root, not just one (see this field's own docs and
    /// `lsp_client_eviction_tests::switching_worktrees_evicts_every_language_client_for_the_old_root_not_just_one`
    /// for the regression coverage this widened key needed). Also drops
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
        let stale_keys = stale_lsp_client_keys(
            &self.lsp_clients.keys().cloned().collect::<Vec<_>>(),
            active_root,
        );
        if stale_keys.is_empty() {
            return;
        }

        let stale_roots: HashSet<PathBuf> =
            stale_keys.iter().map(|(root, _)| root.clone()).collect();
        for root in &stale_roots {
            self.lsp_opened_files.retain(|path| !path.starts_with(root));
        }

        for key in stale_keys {
            let Some(state) = self.lsp_clients.remove(&key) else {
                continue;
            };
            if let LspClientState::Ready(client) = state {
                let server_name = client.name();
                let task = cx.background_executor().spawn(async move {
                    match std::sync::Arc::try_unwrap(client) {
                        Ok(mut client) => {
                            if let Err(err) = client.shutdown() {
                                log::warn!("failed to shut down {server_name}: {err}");
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
            // under `key` even though it's no longer active - harmless: the next eviction pass
            // catches it same as any other stale entry.
        }
    }

    /// Lazily spawns (or reuses) an `lsp_core::LspClient` for `repo_root` running the server for
    /// `extension` - a no-op if a client for this exact `(repo_root, binary)` key already exists
    /// in any state (a previous failure is not retried on every render; this is only called once
    /// per key per [`Self::render_file_view`] pass), or if `extension` has no real LSP identity
    /// at all.
    ///
    /// ## Why `extension`, not an already-built `ServerSpawnConfig`
    ///
    /// This used to take a real `lsp_core::ServerSpawnConfig` built by the caller - which meant
    /// [`Self::render_file_view`] had to call `crate::language::server_spawn_config` (and, for
    /// Python, its real `$PATH` probing via `pyright_initialization_options`) on *every single
    /// repaint*, just to find out whether a spawn was even needed, before this method's own
    /// early-return on an already-present key ever got a chance to short-circuit that work. Now
    /// only a cheap, static [`crate::language::lsp_binary_for_extension`] lookup happens on the
    /// caller's (render) side; the real, possibly-expensive `ServerSpawnConfig` is built here,
    /// inside the `cx.background_executor()` task, and only once this method has confirmed a
    /// fresh spawn genuinely needs to happen - never on the GPUI foreground thread. `extension`
    /// must be the registry's own canonical `&'static str` (e.g. from
    /// `crate::language::entry_for_extension(..).map(|entry| entry.extension)`), not an arbitrary
    /// borrowed slice off a `Path`, since it has to move into a `'static` background task.
    pub(super) fn ensure_lsp_client(
        &mut self,
        repo_root: PathBuf,
        extension: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        let Some(binary) = crate::language::lsp_binary_for_extension(extension) else {
            return;
        };
        let key: LspClientKey = (repo_root.clone(), binary);
        if self.lsp_clients.contains_key(&key) {
            return;
        }
        self.lsp_clients
            .insert(key.clone(), LspClientState::Spawning);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let repo_root = repo_root.clone();
                    async move {
                        // The real `ServerSpawnConfig` (including any `$PATH` probing it does,
                        // e.g. Pyright's `pythonPath` resolution) is built here, off the GPUI
                        // thread - see this method's own docs for why that moved from the caller.
                        match crate::language::server_spawn_config(extension) {
                            Some(config) => lsp_core::LspClient::spawn(&repo_root, config)
                                .map_err(|err| err.to_string()),
                            None => Err(format!(
                                "no LSP server is configured for extension {extension:?}"
                            )),
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(client) => {
                        this.lsp_clients.insert(
                            key.clone(),
                            LspClientState::Ready(std::sync::Arc::new(client)),
                        );
                        this.ensure_lsp_poll_task(cx);
                    }
                    Err(error) => {
                        this.lsp_clients
                            .insert(key.clone(), LspClientState::Failed(error));
                    }
                }
                cx.notify();
            });
        });
        self._lsp_tasks.push(task);
    }

    /// The already-`Ready` client for `path`'s own language, if any - looks up
    /// `crate::language::lsp_binary_for_extension` off `path`'s extension to find the real
    /// [`LspClientKey`] second half, so a hover/go-to-definition request against a `.ts` file
    /// reaches the real typescript-language-server client rather than assuming Rust (this app's
    /// only supported language before Revision R8). `None` for an extension with no LSP identity
    /// at all (`.vue`/`.go`/anything unrecognized) or one whose client isn't `Ready` yet.
    pub(super) fn lsp_client_for_path(
        &self,
        path: &Path,
    ) -> Option<std::sync::Arc<lsp_core::LspClient>> {
        let extension = path.extension().and_then(|ext| ext.to_str());
        let binary = crate::language::lsp_binary_for_extension(extension)?;
        match self
            .lsp_clients
            .get(&(self.file_tree_root.clone(), binary))?
        {
            LspClientState::Ready(client) => Some(client.clone()),
            _ => None,
        }
    }

    /// Sends a `textDocument/didOpen` for `path` tagged with `language_id` (see
    /// `lsp_core::LspClient::did_open`'s own docs on why this varies per extension, not just per
    /// server), once per real path (see [`Self::lsp_opened_files`]'s docs). The file content is
    /// read fresh here (separate from [`Self::file_view_cache`]'s own cached parse; this only
    /// happens once per file open, not per render). Runs on `cx.background_executor()` since
    /// both the file read and the write to the server's stdin are blocking I/O.
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
        language_id: &'static str,
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
                            if let Err(err) = client.did_open(&path, text, 1, language_id) {
                                log::warn!(
                                    "failed to send didOpen for {} to {}: {err}",
                                    path.display(),
                                    client.name()
                                );
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

/// Which of `existing_keys` should be evicted once `active_root` becomes the newly active
/// worktree root: every key whose *root* half isn't `active_root`, regardless of which server
/// binary the other half names - so every language's client for an old root is caught, not just
/// one. Kept gpui-free/pure so the "which keys get removed" bookkeeping is unit-testable without
/// a real `lsp_core::LspClient`, which can only be constructed by genuinely spawning a server.
pub(super) fn stale_lsp_client_keys(
    existing_keys: &[LspClientKey],
    active_root: &Path,
) -> Vec<LspClientKey> {
    existing_keys
        .iter()
        .filter(|(root, _)| root.as_path() != active_root)
        .cloned()
        .collect()
}

/// The language server status this window's status bar shows for the currently open file.
/// Every variant corresponds to a real, distinguishable server state (see [`LspClientState`]'s
/// own docs); there is no variant that fabricates progress this app can't actually observe (a
/// server's real `$/progress` payloads carry richer detail, but this phase doesn't track
/// `$/progress` at all, so [`LspFileStatus::Indexing`]'s coarser "no publishDiagnostics yet"
/// signal is used instead).
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
    fn stale_lsp_client_keys_keeps_only_the_active_root() {
        let keys = vec![
            (PathBuf::from("/a"), "rust-analyzer"),
            (PathBuf::from("/b"), "rust-analyzer"),
            (PathBuf::from("/c"), "rust-analyzer"),
        ];
        let stale = stale_lsp_client_keys(&keys, Path::new("/b"));
        assert_eq!(
            stale,
            vec![
                (PathBuf::from("/a"), "rust-analyzer"),
                (PathBuf::from("/c"), "rust-analyzer"),
            ]
        );
    }

    #[test]
    fn stale_lsp_client_keys_is_empty_when_the_active_root_is_the_only_one() {
        let keys = vec![(PathBuf::from("/a"), "rust-analyzer")];
        let stale = stale_lsp_client_keys(&keys, Path::new("/a"));
        assert!(stale.is_empty());
    }

    /// The real regression test the widened `(PathBuf, &'static str)` key needed (see
    /// `AdeApp::lsp_clients`'s own docs): every *language's* client for an evicted root must be
    /// torn down on a worktree switch, not just whichever one happened to be first in the map -
    /// seeds two entries (standing in for a Rust file and a TypeScript file both having been
    /// opened under the same old worktree) and confirms both are gone after switching away.
    #[test]
    fn stale_lsp_client_keys_catches_every_binary_under_an_evicted_root_not_just_one() {
        let keys = vec![
            (PathBuf::from("/old"), "rust-analyzer"),
            (PathBuf::from("/old"), "typescript-language-server"),
            (PathBuf::from("/new"), "rust-analyzer"),
        ];
        let stale = stale_lsp_client_keys(&keys, Path::new("/new"));
        assert_eq!(
            stale,
            vec![
                (PathBuf::from("/old"), "rust-analyzer"),
                (PathBuf::from("/old"), "typescript-language-server"),
            ],
            "both of the old root's language clients should be caught, not just one"
        );
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
                    .insert((root.clone(), "rust-analyzer"), LspClientState::Spawning);
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
                    app.lsp_clients
                        .contains_key(&(root.clone(), "rust-analyzer")),
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
            app.lsp_clients
                .insert((root, "rust-analyzer"), LspClientState::Spawning);
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

    /// The `AdeApp`-level proof (not just the pure `stale_lsp_client_keys` helper above) that a
    /// worktree switch tears down **every** language's client for the old root - seeds both a
    /// `rust-analyzer` and a `typescript-language-server` entry under the same old worktree
    /// (standing in for a `.rs` and a `.ts` file both having been opened there, the exact real
    /// scenario the widened `(PathBuf, &'static str)` key exists to support) and confirms both
    /// are gone, and only the new root's entry remains, after switching.
    #[gpui::test]
    fn a_worktree_switch_evicts_every_language_client_for_the_old_root_not_just_one(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let worktree_old = tempfile::tempdir().expect("tempdir old");
        let worktree_new = tempfile::tempdir().expect("tempdir new");

        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());

        app.update(cx, |app, _cx| {
            app.worktrees = vec![
                worktree_item(worktree_old.path().to_path_buf()),
                worktree_item(worktree_new.path().to_path_buf()),
            ];
        });

        app.update(cx, |app, cx| {
            app.select_worktree(0, cx);
            let root = app.file_tree_root.clone();
            app.lsp_clients
                .insert((root.clone(), "rust-analyzer"), LspClientState::Spawning);
            app.lsp_clients.insert(
                (root.clone(), "typescript-language-server"),
                LspClientState::Spawning,
            );
            app.lsp_opened_files.insert(root.join("src/main.rs"));
            app.lsp_opened_files.insert(root.join("web/app.ts"));
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.lsp_clients.len(),
                2,
                "both language clients should exist before the switch away"
            );
        });

        app.update(cx, |app, cx| {
            app.select_worktree(1, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert_eq!(
                app.lsp_clients.len(),
                0,
                "both of the old root's language clients should be evicted, not just the first \
                 one found - got: {:?}",
                app.lsp_clients.keys().collect::<Vec<_>>()
            );
            assert!(
                app.lsp_opened_files.is_empty(),
                "lsp_opened_files should have no entries left under the evicted old root"
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
                "no real diagnostic reached AdeApp::file_view_diagnostics within the caller's \
                 real deadline (this helper is shared by callers with different real timeouts - \
                 180s for rust-analyzer, 120s for typescript-language-server - so the message \
                 deliberately doesn't hardcode either one)"
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

    /// The real end-to-end proof that Revision R8's generalization actually reaches
    /// `AdeApp`, not just `lsp_core` directly (that already-thorough proof lives in
    /// `lsp_core::client::tests::typescript_language_server_reports_a_real_diagnostic_for_a_real_type_error`)
    /// - the same `AdeApp::open_file_view` -> `render_center_pane` (-> the old `is_rust` gate,
    /// now `crate::language::server_spawn_config`) -> `ensure_lsp_client` -> `dispatch_did_open`
    /// path the Rust test above exercises, but for a real `.ts` file, proving the extension-based
    /// dispatch that replaced the old boolean gate genuinely reaches a non-Rust language too.
    #[gpui::test]
    fn a_real_typescript_diagnostic_reaches_file_view_diagnostics_through_the_real_app_code_path(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\": {\"strict\": true, \"target\": \"ES2020\"}}\n",
        )
        .expect("write tsconfig.json");
        let main_ts = dir.path().join("main.ts");
        std::fs::write(
            &main_ts,
            "const bad: number = \"not a number\";\nconsole.log(bad);\n",
        )
        .expect("write main.ts");
        // See `lsp_core::client::tests::write_scratch_ts_project`'s own docs for why a real,
        // project-local `npm install typescript@5` is genuinely required in this sandbox, not
        // just conservative.
        let status = std::process::Command::new("npm")
            .args([
                "install",
                "typescript@5",
                "--no-audit",
                "--no-fund",
                "--silent",
            ])
            .current_dir(dir.path())
            .status()
            .expect("npm should be on PATH in this sandbox (real, live network install)");
        assert!(status.success(), "npm install typescript@5 failed");

        let (app, cx) = palette_focus_tests::open_test_app(cx, dir.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_ts.clone(), window, cx);
        });
        cx.run_until_parked();

        let deadline = Instant::now() + Duration::from_secs(120);
        wait_for_real_diagnostics(&app, cx, deadline);

        app.read_with(cx, |app, _| {
            let all_diagnostics: Vec<&diagnostics_view::LineDiagnostic> =
                app.file_view_diagnostics.values().flatten().collect();
            let mismatch = all_diagnostics
                .iter()
                .find(|diagnostic| diagnostic.message.to_lowercase().contains("not assignable"));
            assert!(
                mismatch.is_some(),
                "expected a real diagnostic referencing the genuine TypeScript type mismatch, \
                 got: {all_diagnostics:#?}"
            );

            assert_eq!(
                app.lsp_clients.len(),
                1,
                "exactly one real lsp_clients entry should exist, keyed by \
                 (repo_root, \"typescript-language-server\") - the widened key from a bare root"
            );
            assert!(
                app.lsp_clients
                    .contains_key(&(dir.path().to_path_buf(), "typescript-language-server")),
                "the real entry should be keyed by the real typescript-language-server binary, \
                 not left over from some other server"
            );
        });
    }
}
