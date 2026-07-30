use super::*;
use crate::root::completions::{CompletionsEntry, CompletionsStatus};
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

/// [`AdeApp::lsp_clients`]' real key: a repository root paired with the real server binary
/// running for it - see that field's own docs for why a bare `PathBuf` was widened to this
/// (Revision R8) once more than one language could have a live client under the same root.
pub(super) type LspClientKey = (PathBuf, &'static str);

/// How long after the last keystroke [`AdeApp::schedule_lsp_sync`] waits before sending a real
/// `textDocument/didChange` (and, when the resulting context is completion-worthy, chaining a
/// real `textDocument/completion` request right after it) - matches Zed's own real
/// `LSP_REQUEST_DEBOUNCE_TIMEOUT` (`vendor/zed/crates/editor/src/editor.rs:307`,
/// `Duration::from_millis(50)`), which Zed uses to debounce the *expensive requests* (hover,
/// completions) a buffer edit can trigger.
///
/// That citation needs a real caveat, not a blind copy: per `vendor/zed/crates/editor/src/
/// editor.rs:9566-9608`'s own `on_buffer_event` -> `update_lsp_data`, Zed sends the `didChange`
/// *notification* itself promptly, on every edit, undebounced - only the requests are debounced.
/// This app deliberately debounces the notification too, and that's a considered, verified
/// deviation, not a shortcut: [`lsp_core::LspClient::did_change_full`] always sends
/// **full-document** sync (a `TextDocumentContentChangeEvent` with no `range` - see that method's
/// own docs), which makes coalescing safe in a way Zed's own per-edit *incremental* sync cannot
/// be - an incremental delta that gets silently skipped corrupts the server's reconstructed
/// document, but a full-document replacement event never depends on any earlier one, so sending
/// only the *latest* content a debounce window settles on loses nothing a server needs to know.
/// Reusing Zed's exact 50ms figure keeps this in the same "still feels live while typing" range
/// that number was chosen for, without inventing a second, unverified constant for what is, in
/// this app's case, a safe generalization of the same underlying idea.
const LSP_SYNC_DEBOUNCE: Duration = Duration::from_millis(50);

/// How many extra times [`AdeApp::schedule_lsp_sync`] re-pulls diagnostics if a real
/// `lsp_core::LspClient::pull_diagnostics` call succeeds but reports an empty result right after
/// a real `didChange` - a genuine, live-observed race distinct from the `ServerCancelled` retry
/// `pull_diagnostics` itself already handles internally: a real rust-analyzer's own internal
/// reanalysis can still be catching up to the exact content just sent even when it *doesn't*
/// cancel the pull, answering instead with a real, structurally valid, but stale "no problems"
/// report (observed live, under real parallel-process CPU contention, while building this
/// feature - see `lsp_core::client::tests::did_change_full_then_a_real_pull_reports_a_real_new_diagnostic`'s
/// own docs for the same race caught at the `lsp-core` layer).
///
/// Only ever consulted when [`LspSyncRequest::previous_result_was_non_empty`] is `true`
/// (Revision R8.5b audit finding 2's fix for a real, live-measured bug): retrying *every* real
/// empty pull result, unconditionally, meant that for any genuinely clean file - where an empty
/// result is simply the honest truth, not staleness - every single settled keystroke paid this
/// retry budget's full real ~8s in the common case, live-measured to also gate the real
/// `textDocument/completion` request behind it (see [`AdeApp::schedule_lsp_sync`]'s own docs for
/// why that gating is *also* now independently fixed). "The previous known result for this file
/// was non-empty" is the real, honest signal that a *fresh* empty result is actually suspicious
/// (diagnostics don't just vanish on their own) rather than assumed by default; a file with no
/// prior non-empty result (freshly opened and clean, or this is the very first sync) gets exactly
/// one real pull and accepts whatever it says, trusting the next real sync tick to naturally
/// refresh it rather than pre-emptively distrusting an honest "no problems" answer.
const PULL_DIAGNOSTICS_EMPTY_RETRIES: u32 = 20;
/// Real backoff between [`PULL_DIAGNOSTICS_EMPTY_RETRIES`] re-pulls - together with that count,
/// a real ~8s worst-case total budget, only ever actually paid for a file whose diagnostics were
/// genuinely non-empty a moment ago (see that constant's own docs).
const PULL_DIAGNOSTICS_EMPTY_RETRY_DELAY: Duration = Duration::from_millis(400);

/// The real, snapshotted work [`AdeApp::prepare_lsp_sync`] decides needs to happen once a
/// debounce settles - computed synchronously on the foreground thread (reading
/// [`AdeApp::edit_buffers`]/[`AdeApp::lsp_clients`], which only that thread may touch), then
/// handed to [`AdeApp::schedule_lsp_sync`]'s async continuation to actually execute off-thread.
/// Either field may be `None` independently: a debounce tick can have new content to sync but no
/// completion-worthy context (an edit that isn't near an identifier/trigger character), or vice
/// versa (the caret sits after an identifier but the content already matches what was last sent).
struct LspSyncPlan {
    sync: Option<LspSyncRequest>,
    completion: Option<CompletionRequestPlan>,
}

/// The real snapshot needed to send one `textDocument/didChange` (and, for a pull-capable
/// server, the real diagnostics pull that follows it) - see [`AdeApp::schedule_lsp_sync`]'s own
/// docs for how this is actually used.
struct LspSyncRequest {
    client: std::sync::Arc<lsp_core::LspClient>,
    absolute_path: PathBuf,
    /// The real, full buffer content to send - a single owned clone (Revision R8.5b audit
    /// finding 7's fix: an earlier version independently cloned `buffer.content` up to three
    /// times per debounce tick across this struct's construction and two now-removed early
    /// writes; this is the only clone taken in [`AdeApp::prepare_lsp_sync`] itself, moved
    /// straight into this field with no further copy made there. [`AdeApp::schedule_lsp_sync`]'s
    /// own async continuation still needs a *second* owned copy - one to actually consume in the
    /// real `did_change_full` wire call, one to keep for [`AdeApp::lsp_last_synced_content`]'s
    /// own post-success bookkeeping - so the real total is two clones per genuine sync, not one,
    /// but that's the honest minimum for "send this content" and "remember what was sent" to both
    /// hold their own real owned copy, down from the audit-identified three.
    content: String,
    version: i32,
    /// Whether [`AdeApp::lsp_client_for_path`]'s client's [`lsp_core::LspClient::diagnostics_for_uri`]
    /// already held a real, *non-empty* result for this path just before this sync was planned -
    /// see [`PULL_DIAGNOSTICS_EMPTY_RETRIES`]'s own docs for why this, not an unconditional
    /// retry-on-empty, is what actually gates [`AdeApp::schedule_lsp_sync`]'s post-sync retry
    /// loop (Revision R8.5b audit finding 2). Computed via the cached [`AdeApp::lsp_uri_cache`]
    /// entry when one exists (never a fresh, blocking `uri_for_path` call here - see that field's
    /// own docs); `false` when there's no cached uri yet, the same honest "nothing to distrust
    /// yet" default a freshly opened file gets.
    previous_result_was_non_empty: bool,
}

/// The real snapshot needed to issue one `textDocument/completion` request and apply its result
/// only if nothing superseded it in the meantime - see [`AdeApp::completions_generation`]'s own
/// docs for what `generation` guards against.
struct CompletionRequestPlan {
    client: std::sync::Arc<lsp_core::LspClient>,
    generation: u64,
    params: lsp_core::lsp_types::CompletionParams,
    /// Worktree-relative, matching [`AdeApp::edit_buffers`]'s own key convention.
    relative_path: PathBuf,
}

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
            // `lsp_document_versions` is absolute-path-keyed the same way `lsp_opened_files` is
            // (see that field's own docs) - pruned here for the same reason: a stale root's
            // version numbers are meaningless once its client (and thus its whole document set)
            // is gone, and an unbounded map here would otherwise grow for the life of the window
            // across every worktree ever visited.
            self.lsp_document_versions
                .retain(|path, _| !path.starts_with(root));
            // `lsp_uri_cache` is absolute-path-keyed the same way (see that field's own docs) -
            // pruned here for the same reason, so it doesn't grow unbounded across every
            // worktree ever visited in the window's lifetime either.
            self.lsp_uri_cache.retain(|path, _| !path.starts_with(root));
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
    /// Also computes and caches `path`'s real `file://` [`lsp_core::lsp_types::Uri`] into
    /// [`Self::lsp_uri_cache`] here, off-thread (Revision R8.5b audit finding 8's fix) - a real,
    /// live-reproduced rule violation: an earlier version of [`Self::prepare_lsp_sync`] called
    /// [`lsp_core::LspClient::uri_for_path`] (a blocking `canonicalize()` syscall) inline, on the
    /// GPUI foreground thread, on *every* real debounced sync tick, directly contradicting this
    /// same module's own stated "never acceptable to run inline on the GPUI thread" convention
    /// (see [`Self::schedule_lsp_sync`]'s own docs for the identical rule already being followed
    /// for [`lsp_core::LspClient::diagnostics_for`]'s own internal canonicalize). Computing it
    /// exactly once here - the same real moment `path` is confirmed to have a real, ready LSP
    /// client and is about to have its content read anyway - means [`Self::prepare_lsp_sync`]
    /// never needs to call it at all: a cache miss there (only possible if an edit somehow lands
    /// before this background task resolves) just honestly skips dispatching a completion request
    /// for that one tick, rather than falling back to a second, still-blocking inline computation.
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

        let task = cx.spawn(async move |this, cx| {
            let uri = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
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
                        // Computed here (off the foreground thread), regardless of whether the
                        // read/didOpen above succeeded - a real, valid uri for `path` doesn't
                        // depend on either having gone well, and caching it here is strictly
                        // additive: worst case, a failed didOpen still leaves a usable cache entry
                        // for whenever a client for this path does become genuinely usable.
                        lsp_core::LspClient::uri_for_path(&path).ok()
                    }
                })
                .await;
            if let Some(uri) = uri {
                let _ = this.update(cx, |this, _cx| {
                    this.lsp_uri_cache.insert(path, uri);
                });
            }
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

    /// Debounces a real live `textDocument/didChange` sync for `relative_path`'s buffer,
    /// dispatching a real `textDocument/completion` request alongside it when the settled edit
    /// looks completion-worthy - see [`LSP_SYNC_DEBOUNCE`]'s own docs for why coalescing both
    /// into one debounced step is safe. Called from every real edit call site in
    /// `crate::root::editing` (`replace_text_in_range`/`replace_and_mark_text_in_range`/
    /// backspace/delete), alongside `Self::schedule_rehighlight`.
    ///
    /// A single slot per path in [`AdeApp::_lsp_sync_tasks`] (matching
    /// `Self::schedule_rehighlight`'s own `_rehighlight_tasks` discipline): assigning a fresh
    /// task here drops whatever earlier debounce/in-flight sync+pull cycle was still running for
    /// the same path, so a fast typist can never produce two overlapping real `didChange`/pull
    /// round trips for one file - the same "only the most recent keystroke's work should ever
    /// land" guarantee this project's own history (Revision R3, R5.5) keeps needing.
    ///
    /// ## The real completion request is never gated behind the diagnostics pull
    ///
    /// Revision R8.5b audit finding 2's fix for a real, live-measured bug: an earlier version
    /// dispatched the real `textDocument/completion` request only *after* the whole diagnostics-
    /// pull retry sequence below finished - up to a real, measured ~8s on a genuinely clean file
    /// (see [`PULL_DIAGNOSTICS_EMPTY_RETRIES`]'s own docs), during which real `Enter`/`Up`/`Down`
    /// keystrokes had nothing to act on (compounding finding 1's own bug). The real completion
    /// request is now dispatched as its own, genuinely independent [`Self::_completions_request_task`]
    /// the moment the server is known to have the latest content - either right after a real sync
    /// this tick succeeds, or immediately if there was nothing new to sync in the first place -
    /// rather than being sequenced after the pull loop. It's still dispatched *after*, not
    /// *before*, a real sync that did need to happen: both a `didChange` notification and a
    /// completion request travel over the same real, `Mutex`-guarded stdin pipe (see
    /// `lsp_core::client`'s own module docs), so reversing that order would risk the server
    /// answering completions against stale, pre-edit content - a real correctness bug, not just a
    /// latency one; only the diagnostics-*pull* retry loop, the actual multi-second offender, is
    /// being decoupled here.
    pub(super) fn schedule_lsp_sync(&mut self, relative_path: PathBuf, cx: &mut Context<Self>) {
        let task = cx.spawn({
            let relative_path = relative_path.clone();
            async move |this, cx| {
                cx.background_executor().timer(LSP_SYNC_DEBOUNCE).await;
                let Ok(Some(plan)) =
                    this.update(cx, |this, cx| this.prepare_lsp_sync(&relative_path, cx))
                else {
                    return;
                };
                let LspSyncPlan { sync, completion } = plan;

                // The real sync, if this tick has new content to send - always awaited (not
                // spawned as a separate task) before the completion request below, for the real
                // wire-ordering correctness reason explained in this method's own docs.
                let mut server_has_latest_content = true;
                let mut pull_context = None;
                if let Some(request) = sync {
                    let LspSyncRequest {
                        client,
                        absolute_path,
                        content,
                        version,
                        previous_result_was_non_empty,
                    } = request;
                    let sync_client = client.clone();
                    let sync_path = absolute_path.clone();
                    let content_for_wire = content.clone();
                    let sync_result = cx
                        .background_executor()
                        .spawn(async move {
                            sync_client.did_change_full(&sync_path, content_for_wire, version)
                        })
                        .await;
                    server_has_latest_content = sync_result.is_ok();
                    if sync_result.is_ok() {
                        // Written only now, after `did_change_full` genuinely returned `Ok`
                        // (Revision R8.5b audit finding 6's fix) - an earlier version wrote
                        // `lsp_last_synced_content` at *plan* time, before the send was even
                        // attempted, which meant a failed send still left this bookkeeping
                        // (and thus `Self::render_file_view`'s own `sync_pending` banner)
                        // confidently, wrongly claiming the server had content it never actually
                        // received.
                        let record_path = relative_path.clone();
                        let _ = this.update(cx, |this, _cx| {
                            this.lsp_last_synced_content
                                .insert(record_path.clone(), content);
                            this.lsp_synced_version.insert(record_path, version);
                        });
                        pull_context = Some((
                            client,
                            absolute_path,
                            version,
                            previous_result_was_non_empty,
                        ));
                    }
                }

                // Real Completions dispatch - see this method's own docs for why this is a
                // genuinely independent task, not awaited inline, and dispatched here rather than
                // after the diagnostics-pull sequence further below.
                if server_has_latest_content {
                    if let Some(request) = completion {
                        // A genuinely independent task (Revision R8.5b audit finding 2), not
                        // awaited inline here - `cx.spawn` inside an already-async `Context::spawn`
                        // continuation only takes a plain `AsyncFnOnce(&mut AsyncApp)` (unlike the
                        // outer, entity-scoped `Context::spawn` that provided `this`/`cx` above),
                        // so the already-weak `this` handle is cloned and moved in explicitly
                        // rather than re-received as a closure parameter.
                        let completion_this = this.clone();
                        let completion_task = cx.spawn(async move |cx| {
                            let CompletionRequestPlan {
                                client,
                                generation,
                                params,
                                relative_path,
                            } = request;
                            let result = cx
                                .background_executor()
                                .spawn(async move {
                                    client.request::<lsp_core::lsp_types::request::Completion>(
                                        params,
                                        LSP_QUERY_TIMEOUT,
                                    )
                                })
                                .await;
                            let _ = completion_this.update(cx, |this, cx| {
                                this.apply_completion_result(
                                    &relative_path,
                                    generation,
                                    result,
                                    cx,
                                );
                            });
                        });
                        let _ = this.update(cx, |this, _cx| {
                            this._completions_request_task = Some(completion_task);
                        });
                    }
                }

                let Some((client, absolute_path, version, previous_result_was_non_empty)) =
                    pull_context
                else {
                    return;
                };
                // Real, live-verified fact (Revision R8.5b), not a guess: a real, installed
                // rust-analyzer was found, by live probing while building this feature, to
                // *push* a real `publishDiagnostics` notification only once - right after
                // `didOpen` - and never again on its own initiative after a subsequent real
                // `didChange`, even though it advertises `textDocumentSync` support for one;
                // real, updated diagnostics only ever arrive there via an actively *pulled*
                // `textDocument/diagnostic` request. `lsp_core::LspClient::
                // supports_diagnostic_pull` reads each real server's own advertised
                // `diagnostic_provider` capability to decide whether this extra pull is
                // needed at all, rather than hardcoding a per-server list - real, live-tested
                // end to end against all three of this app's supported servers (`crate::root::
                // lsp::lsp_diagnostics_wiring_tests`), each correctly exercising whichever
                // real path its own advertised capability calls for. A real pull is a genuine
                // no-op for a server that never advertises the capability, so it's skipped
                // rather than always attempted. Errors are intentionally swallowed here (not
                // surfaced as a user-facing failure): a failed pull just means diagnostics
                // stay whatever they were until the *next* sync tick tries again, which
                // `Self::render_file_view`'s own `sync_pending` banner already communicates
                // honestly - this app has no separate "diagnostics refresh failed" affordance
                // for this phase's scope, and inventing one for a single best-effort
                // background refresh isn't worth it.
                if client.supports_diagnostic_pull() {
                    // See `PULL_DIAGNOSTICS_EMPTY_RETRIES`'s own docs for the real,
                    // live-observed race this bounded retry-on-empty closes, and for why it's
                    // only even entered at all when `previous_result_was_non_empty` (Revision
                    // R8.5b audit finding 2) - on a genuinely clean file this loop now runs
                    // exactly once, not up to 21 times. The actual blocking LSP call is
                    // still always off the foreground thread (a fresh `cx.background_executor()
                    // .spawn()` per attempt); only the real, deterministic-clock-aware
                    // `cx.background_executor().timer()` between attempts runs in this outer
                    // async task, matching this file's own established convention (see
                    // `AdeApp::ensure_lsp_poll_task`'s identical `timer(..)`-then-recheck
                    // shape) rather than a second, unverified sleep mechanism from inside a
                    // `'static` background closure that has no `cx` of its own to time with.
                    let max_retries = if previous_result_was_non_empty {
                        PULL_DIAGNOSTICS_EMPTY_RETRIES
                    } else {
                        0
                    };
                    for attempt in 0..=max_retries {
                        let pull_client = client.clone();
                        let pull_path = absolute_path.clone();
                        // The real pull *and* the real "was the result empty" check both run
                        // off the foreground thread together (the latter's own
                        // `LspClient::diagnostics_for` does a real, if cheap,
                        // `std::fs::canonicalize` syscall internally - never acceptable to
                        // run inline on the GPUI thread per this crate's own convention).
                        // `version` is threaded through so a real, late-landing result for an
                        // older version can never clobber a fresher one already applied
                        // (Revision R8.5b audit finding 5 - see `lsp_core::LspClient::
                        // pull_diagnostics`'s own docs for where that guard actually lives).
                        let outcome = cx
                            .background_executor()
                            .spawn(async move {
                                pull_client
                                    .pull_diagnostics(&pull_path, version, LSP_QUERY_TIMEOUT)
                                    .ok()?;
                                Some(
                                    pull_client
                                        .diagnostics_for(&pull_path)
                                        .is_some_and(|diagnostics| diagnostics.is_empty()),
                                )
                            })
                            .await;
                        if outcome.is_some() {
                            // A real, successful pull answer landed for this exact version - the
                            // real "the server has genuinely answered for this edit" confirmation
                            // `Self::render_file_view`'s own `sync_pending` banner now waits on
                            // (Revision R8.5b audit finding 6), not just the send itself.
                            // `.max(..)` so a real, late-arriving confirmation for an older
                            // version (the same reordering finding 5 guards `pull_diagnostics`'s
                            // own map against) can never regress this back down either.
                            let record_path = relative_path.clone();
                            let _ = this.update(cx, |this, _cx| {
                                let confirmed = this
                                    .lsp_diagnostics_confirmed_version
                                    .entry(record_path)
                                    .or_insert(version);
                                *confirmed = (*confirmed).max(version);
                            });
                        }
                        match outcome {
                            Some(true) if attempt < max_retries => {
                                cx.background_executor()
                                    .timer(PULL_DIAGNOSTICS_EMPTY_RETRY_DELAY)
                                    .await;
                            }
                            _ => break,
                        }
                    }
                } else {
                    // No pull needed - this server pushes fresh diagnostics on its own timeline
                    // (or doesn't advertise `diagnostic_provider` at all), so there's no further
                    // real confirmation step this app's own side can wait on; the successful send
                    // itself is the honest "confirmed" signal in that case.
                    let record_path = relative_path.clone();
                    let _ = this.update(cx, |this, _cx| {
                        let confirmed = this
                            .lsp_diagnostics_confirmed_version
                            .entry(record_path)
                            .or_insert(version);
                        *confirmed = (*confirmed).max(version);
                    });
                }
            }
        });
        self._lsp_sync_tasks.insert(relative_path, task);
    }

    /// The synchronous, foreground half of [`Self::schedule_lsp_sync`]'s debounced work - reads
    /// [`AdeApp::edit_buffers`]/[`AdeApp::lsp_clients`] fresh (this is the *first* foreground step
    /// after the debounce timer, so this is always the real, current state, never a stale
    /// snapshot from when the debounce was armed), decides what real work is owed, and performs
    /// every mutation that work implies (bumping [`AdeApp::lsp_document_versions`],
    /// seeding/advancing [`AdeApp::completions`]) before handing the actual I/O off to
    /// [`Self::schedule_lsp_sync`]'s async continuation. `None` when there's nothing real to do
    /// at all (no buffer, no ready client, content already in sync and no completion-worthy
    /// context).
    ///
    /// Deliberately does **not** write [`AdeApp::lsp_last_synced_content`]/[`AdeApp::
    /// lsp_synced_version`] here (Revision R8.5b audit finding 6's fix - see [`Self::
    /// schedule_lsp_sync`]'s own docs for where that write actually happens now, and why): this
    /// is *plan* time, before the real `did_change_full` send is even attempted, let alone known
    /// to have succeeded.
    fn prepare_lsp_sync(
        &mut self,
        relative_path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<LspSyncPlan> {
        let buffer = self.edit_buffers.get(relative_path)?;
        let absolute_path = buffer.path.clone();
        // A single owned clone, reused for everything below (Revision R8.5b audit finding 7's
        // fix) - see `LspSyncRequest::content`'s own docs for the real, honest minimum this was
        // brought down to.
        let content = buffer.content.clone();
        let cursor = buffer.cursor_offset();
        let (line, _) = buffer.line_col_for_offset(cursor);
        let line_utf16_start = buffer.utf16_line_starts.get(line).copied().unwrap_or(0);
        let character = (buffer
            .offset_to_utf16(cursor)
            .saturating_sub(line_utf16_start)) as u32;
        let position = lsp_core::lsp_types::Position {
            line: line as u32,
            character,
        };

        let Some(client) = self.lsp_client_for_path(&absolute_path) else {
            // No ready client for this file's language (not spawned yet, still spawning, or
            // failed) - nothing real to sync or complete against. A stale popup from *before* the
            // client went away (e.g. a worktree switch mid-flight) shouldn't linger either.
            if self
                .completions
                .as_ref()
                .is_some_and(|entry| entry.path == relative_path)
            {
                self.dismiss_completions();
                cx.notify();
            }
            return None;
        };

        let content_unchanged = self.lsp_last_synced_content.get(relative_path) == Some(&content);
        let should_sync = !content_unchanged && client.supports_document_sync();

        let char_before_cursor = crate::completion_view::char_before(&content, cursor);
        let trigger_characters = client.completion_trigger_characters();
        let completion_context =
            crate::completion_view::completion_trigger(char_before_cursor, &trigger_characters);

        if !should_sync && completion_context.is_none() {
            if self
                .completions
                .as_ref()
                .is_some_and(|entry| entry.path == relative_path)
            {
                // The context that justified the popup no longer holds (e.g. the user just typed
                // a space, or a delimiter that isn't a real advertised trigger character) - dismiss
                // it rather than let it silently go stale with no further edit ever refreshing it.
                self.dismiss_completions();
                cx.notify();
            }
            return None;
        }

        // The real, cached `file://` uri for `absolute_path` - see [`Self::lsp_uri_cache`]'s own
        // docs (Revision R8.5b audit finding 8) for why this is a cache lookup, never a fresh,
        // blocking `uri_for_path` call here on the GPUI foreground thread. Used both for building
        // the real completion request below and (Revision R8.5b audit finding 2) for checking
        // whether this path's last known diagnostics result was non-empty, without a second
        // blocking call.
        let cached_uri = self.lsp_uri_cache.get(&absolute_path).cloned();
        let previous_result_was_non_empty = cached_uri
            .as_ref()
            .and_then(|uri| client.diagnostics_for_uri(uri))
            .is_some_and(|diagnostics| !diagnostics.is_empty());

        let sync = if should_sync {
            let version_slot = self
                .lsp_document_versions
                .entry(absolute_path.clone())
                .or_insert(1);
            *version_slot += 1;
            let version = *version_slot;
            Some(LspSyncRequest {
                client: client.clone(),
                absolute_path: absolute_path.clone(),
                content,
                version,
                previous_result_was_non_empty,
            })
        } else {
            None
        };

        let completion = match (completion_context, cached_uri) {
            (Some(context), Some(uri)) => {
                self.completions_generation = self.completions_generation.wrapping_add(1);
                self.completions = Some(CompletionsEntry {
                    path: relative_path.to_path_buf(),
                    status: CompletionsStatus::Loading,
                });
                cx.notify();
                let params = lsp_core::lsp_types::CompletionParams {
                    text_document_position: lsp_core::lsp_types::TextDocumentPositionParams {
                        text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri },
                        position,
                    },
                    work_done_progress_params: lsp_core::lsp_types::WorkDoneProgressParams::default(
                    ),
                    partial_result_params: lsp_core::lsp_types::PartialResultParams::default(),
                    context: Some(context),
                };
                Some(CompletionRequestPlan {
                    client,
                    generation: self.completions_generation,
                    params,
                    relative_path: relative_path.to_path_buf(),
                })
            }
            // A completion-worthy context with no cached uri yet (only possible in the narrow
            // window before `Self::dispatch_did_open`'s own background task resolves) is an
            // honest "nothing to dispatch this tick" - the next debounce tick will very likely
            // find the cache populated, rather than falling back to a second, still-blocking
            // inline `uri_for_path` call here.
            _ => None,
        };

        Some(LspSyncPlan { sync, completion })
    }

    /// Applies a real, completed `textDocument/completion` response - see
    /// [`AdeApp::completions_generation`]'s own docs for the real stale-response race this
    /// `generation` check closes (an in-flight request whose *task* wasn't cancelled, because the
    /// user dismissed the popup via `Escape` rather than a further edit, must not resurrect it).
    /// Also refuses to apply a response for a path that's no longer what [`AdeApp::completions`]
    /// is even showing - defensive in the same spirit as `Self::request_hover`'s own
    /// `still_current` check, though `generation` alone already covers every real path this
    /// method is reachable from.
    fn apply_completion_result(
        &mut self,
        relative_path: &Path,
        generation: u64,
        result: Result<Option<lsp_core::lsp_types::CompletionResponse>, lsp_core::LspError>,
        cx: &mut Context<Self>,
    ) {
        if self.completions_generation != generation {
            return;
        }
        let matches_current = self
            .completions
            .as_ref()
            .is_some_and(|entry| entry.path == relative_path);
        if !matches_current {
            return;
        }

        let new_state = match result {
            Ok(Some(response)) => {
                let items = match response {
                    lsp_core::lsp_types::CompletionResponse::Array(items) => items,
                    lsp_core::lsp_types::CompletionResponse::List(list) => list.items,
                };
                if items.is_empty() {
                    None
                } else {
                    Some(CompletionsEntry {
                        path: relative_path.to_path_buf(),
                        status: CompletionsStatus::Ready { items, selected: 0 },
                    })
                }
            }
            Ok(None) => None,
            Err(err) => Some(CompletionsEntry {
                path: relative_path.to_path_buf(),
                status: CompletionsStatus::Failed(err.to_string()),
            }),
        };
        self.completions = new_state;
        cx.notify();
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
        // A real, honest degrade (Revision R8.5b audit finding 9's fix) for a `Ready` client
        // whose underlying process has actually died out from under it - see
        // `lsp_core::LspClient::is_connection_alive`'s own docs for how/why this is tracked.
        // Reported via the same `Failed` variant a spawn/handshake failure already uses (real,
        // honest text explaining *why*, not a fabricated distinct status this phase's scope
        // doesn't otherwise need) rather than silently continuing to report `Indexing`/
        // `Analyzed` off a client that will never answer another real request again.
        Some(LspClientState::Ready(client)) if !client.is_connection_alive() => {
            LspFileStatus::Failed(format!(
                "{}'s connection was lost (the process exited unexpectedly)",
                client.name()
            ))
        }
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
    use gpui::{Entity, EntityInputHandler, TestAppContext, VisualTestContext};
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

    /// Generic real-time poll, shared by every Revision R8.5b test below: repeatedly re-renders
    /// the centre pane (the real trigger for `AdeApp::ensure_lsp_client`/`dispatch_did_open`/
    /// diagnostics indexing to progress) and drains the deterministic test executor, checking
    /// `predicate` against the live `AdeApp` after each pass, until it's true or `deadline`
    /// passes - the same real polling shape [`wait_for_real_diagnostics`] already established,
    /// generalized so these tests can wait for a live completions popup or a specific diagnostic
    /// message, not just "any diagnostic at all".
    fn wait_until(
        app: &Entity<AdeApp>,
        cx: &mut VisualTestContext,
        deadline: Instant,
        message: &str,
        predicate: impl Fn(&AdeApp) -> bool,
    ) {
        loop {
            app.update(cx, |app, cx| {
                app.render_center_pane(cx);
            });
            cx.run_until_parked();
            if app.read_with(cx, |app, _| predicate(app)) {
                return;
            }
            assert!(Instant::now() < deadline, "{message}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Drives a real edit through the exact same code path a real keystroke does
    /// (`EntityInputHandler::replace_text_in_range`, which is what `crate::root::editing`'s own
    /// `on_key_down`/IME plumbing ultimately calls), then advances the deterministic test clock
    /// past [`LSP_SYNC_DEBOUNCE`] and drains the executor so the real, debounced
    /// `AdeApp::schedule_lsp_sync` task this edit armed actually fires (mirrors
    /// `crate::root::editing::editing_tests`' own `REHIGHLIGHT_DEBOUNCE` advance-then-park
    /// pattern for the sibling re-highlight debounce).
    fn type_text(app: &Entity<AdeApp>, cx: &mut VisualTestContext, offset: usize, text: &str) {
        app.update_in(cx, |app, window, cx| {
            let relative = app
                .active_editable_path()
                .expect("a real editable File view tab should be active");
            let buffer = app.edit_buffers.get_mut(&relative).expect("a real buffer");
            buffer.move_to(offset);
            app.replace_text_in_range(None, text, window, cx);
        });
        cx.background_executor
            .advance_clock(LSP_SYNC_DEBOUNCE + Duration::from_millis(50));
        cx.run_until_parked();
    }

    /// The real, live proof Revision R8.5b exists to deliver, for `rust-analyzer`: opening a
    /// clean real file, then making a real *unsaved* edit that introduces a genuine type error,
    /// reaches a real new diagnostic through nothing but this app's own real
    /// `AdeApp::schedule_lsp_sync` -> `lsp_core::LspClient::did_change_full` path - not a saved-
    /// disk-content reload, and not a synthetic notification. The same real, indexed client is
    /// then reused (no second spawn) to prove real, live Completions: typing a real partial
    /// identifier reaches a real `textDocument/completion` response, and accepting it splices the
    /// real chosen item's text into the real buffer via `EditBuffer::replace_range`.
    #[gpui::test]
    fn rust_analyzer_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions(
        cx: &mut TestAppContext,
    ) {
        let project = write_scratch_project(
            "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        let indexed_deadline = Instant::now() + Duration::from_secs(180);
        wait_until(
            &app,
            cx,
            indexed_deadline,
            "rust-analyzer never reported a clean (zero-error) baseline for the real, \
             unedited scratch file within the deadline",
            |app| app.file_view_error_count == Some(0),
        );

        // A real, unsaved edit - inserted, not saved to disk - introducing a genuine type
        // mismatch on a fresh line.
        let insert_at = "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n".len();
        type_text(
            &app,
            cx,
            insert_at,
            "\nfn bad() -> i32 {\n    \"not a number\"\n}\n",
        );
        app.read_with(cx, |app, _| {
            assert!(
                app.edit_buffers
                    .values()
                    .next()
                    .expect("a real buffer")
                    .is_dirty(),
                "the edit must be a real, genuinely unsaved one - this proof is specifically \
                 that live sync doesn't depend on a save"
            );
        });

        let diagnostic_deadline = Instant::now() + Duration::from_secs(180);
        wait_until(
            &app,
            cx,
            diagnostic_deadline,
            "no real diagnostic referencing the genuine new type mismatch arrived within the \
             deadline after a real live didChange sync",
            |app| {
                app.file_view_diagnostics
                    .values()
                    .flatten()
                    .any(|diagnostic| {
                        let message = diagnostic.message.to_lowercase();
                        message.contains("mismatched")
                            || (message.contains("expected") && message.contains("i32"))
                    })
            },
        );

        // The same real, already-`Ready` client (no second spawn) now proves real, live
        // completions: a fresh line with a genuine partial identifier - and, at the same time,
        // Revision R8.5b audit finding 2's own regression coverage: the earlier `bad()` mismatch
        // above was never fixed, so a real `client.diagnostics_for_uri` result for this file is
        // already known, confirmed non-empty *before* this edit - exactly the condition under
        // which `AdeApp::schedule_lsp_sync`'s post-sync diagnostics-pull retry loop is eligible to
        // retry up to `PULL_DIAGNOSTICS_EMPTY_RETRIES` times (real behavior this test doesn't
        // fake). `type_text` only ever advances the deterministic test clock by
        // `LSP_SYNC_DEBOUNCE` - never again after this - so if the real completion request were
        // still sequenced *after* that whole pull sequence (the pre-fix bug), and the pull
        // sequence needed even one real retry, its own `cx.background_executor()
        // .timer(PULL_DIAGNOSTICS_EMPTY_RETRY_DELAY)` wait would never fire on this frozen
        // deterministic clock, and `app.completions` could never reach `Ready` here at all - it
        // would hang until this test's own real, wall-clock deadline below and fail. Reaching
        // `Ready` (bounded, additionally, by a real wall-clock budget well under the real ~8s the
        // old, fully-serialized worst case could add on top of the real completion round trip
        // itself) is the real, live proof the completion request no longer depends on that pull
        // sequence completing, retrying, or its own timer ever advancing.
        let relative = PathBuf::from("src/main.rs");
        let completion_trigger_offset = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .expect("a real buffer")
                .content
                .len()
        });
        let completion_dispatch_started = Instant::now();
        type_text(
            &app,
            cx,
            completion_trigger_offset,
            "\nfn call() {\n    prin",
        );

        let completion_deadline = Instant::now() + Duration::from_secs(60);
        wait_until(
            &app,
            cx,
            completion_deadline,
            "no real completions ever arrived for the genuine \"prin\" prefix within the \
             deadline - if this hangs, the real completion request may have regressed back to \
             being sequenced after the diagnostics-pull retry loop, whose own retry timer this \
             test deliberately never advances past the initial debounce",
            |app| {
                app.completions
                    .as_ref()
                    .is_some_and(|entry| matches!(&entry.status, CompletionsStatus::Ready { .. }))
            },
        );
        assert!(
            completion_dispatch_started.elapsed() < Duration::from_secs(20),
            "the real completion request reaching Ready took {:?} of real wall-clock time - \
             expected well under the real ~8s the pre-fix design's fully-serialized diagnostics-\
             pull retry sequence alone could add on top of the completion round trip itself \
             (Revision R8.5b audit finding 2)",
            completion_dispatch_started.elapsed()
        );
        app.read_with(cx, |app, _| {
            let entry = app.completions.as_ref().expect("checked above");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("checked above");
            };
            assert!(
                items.iter().any(|item| item.label.contains("println")),
                "expected a real completion item for the genuine \"println\" macro among \
                 rust-analyzer's own real response, got: {:?}",
                items.iter().map(|item| &item.label).collect::<Vec<_>>()
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.handle_completions_accept_action(&CompletionsAccept, window, cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let content = &app
                .edit_buffers
                .get(&relative)
                .expect("a real buffer")
                .content;
            assert!(
                content.contains("println"),
                "accepting the real completion should have spliced its real text into the \
                 real buffer, got: {content:?}"
            );
            assert!(
                !content.contains("prinprintln") && !content.contains("println!ln"),
                "the already-typed real prefix (\"prin\") must be replaced, not duplicated \
                 alongside the accepted real completion text, got: {content:?}"
            );
        });
    }

    /// The same real, live proof as the rust-analyzer test above, for `typescript-language-server`
    /// - see `crate::language`'s own docs on why `npm install typescript@5` is a genuine, real
    /// project-local requirement in this sandbox, not conservative caution.
    #[gpui::test]
    fn typescript_language_server_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\": {\"strict\": true, \"target\": \"ES2020\"}}\n",
        )
        .expect("write tsconfig.json");
        let main_ts = dir.path().join("main.ts");
        let baseline = "const ok: number = 1;\nconsole.log(ok);\n";
        std::fs::write(&main_ts, baseline).expect("write main.ts");
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

        let indexed_deadline = Instant::now() + Duration::from_secs(120);
        wait_until(
            &app,
            cx,
            indexed_deadline,
            "typescript-language-server never reported a clean (zero-error) baseline within \
             the deadline",
            |app| app.file_view_error_count == Some(0),
        );

        type_text(
            &app,
            cx,
            baseline.len(),
            "\nconst bad: number = \"not a number\";\n",
        );
        app.read_with(cx, |app, _| {
            assert!(app
                .edit_buffers
                .values()
                .next()
                .expect("a real buffer")
                .is_dirty());
        });

        let diagnostic_deadline = Instant::now() + Duration::from_secs(120);
        wait_until(
            &app,
            cx,
            diagnostic_deadline,
            "no real diagnostic referencing the genuine new TypeScript type mismatch arrived \
             within the deadline after a real live didChange sync",
            |app| {
                app.file_view_diagnostics
                    .values()
                    .flatten()
                    .any(|diagnostic| diagnostic.message.to_lowercase().contains("not assignable"))
            },
        );

        let relative = PathBuf::from("main.ts");
        let completion_trigger_offset = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .expect("a real buffer")
                .content
                .len()
        });
        type_text(&app, cx, completion_trigger_offset, "\nconsol");

        let completion_deadline = Instant::now() + Duration::from_secs(60);
        wait_until(
            &app,
            cx,
            completion_deadline,
            "no real completions ever arrived for the genuine \"consol\" prefix within the \
             deadline",
            |app| {
                app.completions
                    .as_ref()
                    .is_some_and(|entry| matches!(&entry.status, CompletionsStatus::Ready { .. }))
            },
        );
        app.read_with(cx, |app, _| {
            let entry = app.completions.as_ref().expect("checked above");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("checked above");
            };
            assert!(
                items.iter().any(|item| item.label.contains("console")),
                "expected a real completion item for the genuine \"console\" global among \
                 typescript-language-server's own real response, got: {:?}",
                items.iter().map(|item| &item.label).collect::<Vec<_>>()
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.handle_completions_accept_action(&CompletionsAccept, window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let content = &app
                .edit_buffers
                .get(&relative)
                .expect("a real buffer")
                .content;
            assert!(
                content.contains("console"),
                "accepting the real completion should have spliced its real text into the \
                 real buffer, got: {content:?}"
            );
        });
    }

    /// The same real, live proof as the two tests above, for `pyright-langserver`.
    #[gpui::test]
    fn pyright_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let main_py = dir.path().join("main.py");
        let baseline = "ok: int = 1\nprint(ok)\n";
        std::fs::write(&main_py, baseline).expect("write main.py");

        let (app, cx) = palette_focus_tests::open_test_app(cx, dir.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_py.clone(), window, cx);
        });
        cx.run_until_parked();

        let indexed_deadline = Instant::now() + Duration::from_secs(120);
        wait_until(
            &app,
            cx,
            indexed_deadline,
            "pyright-langserver never reported a clean (zero-error) baseline within the \
             deadline",
            |app| app.file_view_error_count == Some(0),
        );

        type_text(&app, cx, baseline.len(), "\nbad: int = \"not a number\"\n");
        app.read_with(cx, |app, _| {
            assert!(app
                .edit_buffers
                .values()
                .next()
                .expect("a real buffer")
                .is_dirty());
        });

        let diagnostic_deadline = Instant::now() + Duration::from_secs(120);
        wait_until(
            &app,
            cx,
            diagnostic_deadline,
            "no real diagnostic referencing the genuine new Python type mismatch arrived \
             within the deadline after a real live didChange sync",
            |app| {
                app.file_view_diagnostics
                    .values()
                    .flatten()
                    .any(|diagnostic| {
                        let message = diagnostic.message.to_lowercase();
                        message.contains("not assignable") || message.contains("incompatible")
                    })
            },
        );

        let relative = PathBuf::from("main.py");
        let completion_trigger_offset = app.read_with(cx, |app, _| {
            app.edit_buffers
                .get(&relative)
                .expect("a real buffer")
                .content
                .len()
        });
        type_text(&app, cx, completion_trigger_offset, "\npri");

        let completion_deadline = Instant::now() + Duration::from_secs(60);
        wait_until(
            &app,
            cx,
            completion_deadline,
            "no real completions ever arrived for the genuine \"pri\" prefix within the \
             deadline",
            |app| {
                app.completions
                    .as_ref()
                    .is_some_and(|entry| matches!(&entry.status, CompletionsStatus::Ready { .. }))
            },
        );
        app.read_with(cx, |app, _| {
            let entry = app.completions.as_ref().expect("checked above");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("checked above");
            };
            assert!(
                items.iter().any(|item| item.label.contains("print")),
                "expected a real completion item for the genuine \"print\" builtin among \
                 pyright-langserver's own real response, got: {:?}",
                items.iter().map(|item| &item.label).collect::<Vec<_>>()
            );
        });

        app.update_in(cx, |app, window, cx| {
            app.handle_completions_accept_action(&CompletionsAccept, window, cx);
        });
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let content = &app
                .edit_buffers
                .get(&relative)
                .expect("a real buffer")
                .content;
            assert!(
                content.contains("print"),
                "accepting the real completion should have spliced its real text into the \
                 real buffer, got: {content:?}"
            );
        });
    }
}
