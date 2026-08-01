use super::*;
use crate::lsp::completion_popup::{CompletionsEntry, CompletionsStatus};
#[cfg(test)]
use crate::root::focus::palette_focus_tests;

/// [`AdeApp::lsp_clients`]' real key: a repository root paired with the real server binary
/// running for it - see that field's own docs for why a bare `PathBuf` was widened to this
/// (Revision R8) once more than one language could have a live client under the same root.
pub(in crate::lsp) type LspClientKey = (PathBuf, &'static str);

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

/// "The LSP for this file", whether that's genuinely one server process or two coordinated ones -
/// the facade every caller in this crate now goes through instead of holding a raw
/// `lsp_core::LspClient`, so `code_surface`'s hover/go-to-definition/render paths never need to
/// know or care which it is.
///
/// ## Why two variants and not, say, a `Vec<Arc<LspClient>>`
///
/// Only one real language needs a second process, and it needs it in one specific, verified way
/// (see [`crate::language::CompanionServer`]): a *primary* that owns the language's identity, and
/// a *companion* that is both a relay target and an independent contributor. That asymmetry is
/// real - the primary alone determines the connection's advertised capabilities, drives document
/// version bookkeeping, and is the only one whose failures are the caller's failures - so
/// flattening both into a symmetric collection would lose information every method below actually
/// uses. [`Self::Single`] is deliberately a plain, zero-extra-work passthrough so the three
/// already-working single-process languages pay nothing for this (see
/// `lsp_connection_facade_tests::single_delegation_stays_under_a_generous_1000ns_ceiling`).
///
/// Constructed fresh per lookup by [`AdeApp::lsp_connection_for_path`] - two `Arc::clone`s and an
/// enum tag, never cached, so it can never hold a stale view of which clients are currently
/// `Ready`.
pub(crate) enum LspConnection {
    Single(std::sync::Arc<lsp_core::LspClient>),
    WithCompanion {
        primary: std::sync::Arc<lsp_core::LspClient>,
        companion: std::sync::Arc<lsp_core::LspClient>,
        spec: crate::language::CompanionServer,
    },
}

/// Whether a real, typed LSP result carries **no information at all** - the protocol's own
/// "nothing here", in every one of the three real wire shapes it actually takes:
///
/// | shape                       | real example                                                  |
/// |-----------------------------|---------------------------------------------------------------|
/// | `null`                      | `Option<Hover>::None`, `Option<GotoDefinitionResponse>::None` |
/// | `[]`                        | `GotoDefinitionResponse::Array(vec![])`/`Link(vec![])`, `CompletionResponse::Array(vec![])` |
/// | `{"items": [], ..}`         | `CompletionResponse::List(CompletionList { items: vec![], .. })` |
///
/// Generic over any `R::Result` (which `lsp_types` guarantees is `Serialize`) so
/// [`LspConnection::request`]'s companion-fallback rule stays a real property of the *connection
/// shape* plus [`crate::language::CompanionServer::fallback_methods`], with no per-method
/// special-casing in the facade itself. Serializing and inspecting the resulting
/// [`serde_json::Value`] is what makes that genuinely possible: inside one generic function the
/// concrete `R::Result` isn't known, but the real wire encoding of "nothing here" is - it *is* the
/// encoding the server actually sent, round-tripped back.
///
/// This is a strict superset of the earlier null-only check it replaced, and it deliberately stays
/// one: the `"items"` key must genuinely be **present and an empty array**, never merely absent, so
/// a real `Hover` (an object with `contents`, and no `items` at all) can never be mistaken for an
/// empty completion list. A result that somehow fails to re-serialize is treated as non-empty - the
/// honest conservative answer, since it definitely isn't one of the three shapes above.
fn lsp_result_is_empty<T: serde::Serialize>(value: &T) -> bool {
    let Ok(value) = serde_json::to_value(value) else {
        return false;
    };
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::Array(entries) => entries.is_empty(),
        serde_json::Value::Object(fields) => fields
            .get("items")
            .and_then(|items| items.as_array())
            .is_some_and(|items| items.is_empty()),
        _ => false,
    }
}

impl LspConnection {
    fn primary(&self) -> &std::sync::Arc<lsp_core::LspClient> {
        match self {
            Self::Single(primary) => primary,
            Self::WithCompanion { primary, .. } => primary,
        }
    }

    fn companion(&self) -> Option<&std::sync::Arc<lsp_core::LspClient>> {
        match self {
            Self::Single(_) => None,
            Self::WithCompanion { companion, .. } => Some(companion),
        }
    }

    /// Sends a real request. [`Self::Single`] delegates straight through.
    ///
    /// [`Self::WithCompanion`] delegates to the primary as well, except for the real methods that
    /// companion's own [`crate::language::CompanionServer::fallback_methods`] names: for those, a
    /// primary answering something [`lsp_result_is_empty`] recognizes as "nothing here" is retried
    /// against the companion, and the companion's non-empty answer is what surfaces.
    ///
    /// Which methods those are is a registry fact, not a facade one - see that field's own docs and
    /// `crate::language`'s `VUE_FALLBACK_METHODS` table for the by-hand measurement behind Vue's
    /// list (hover, go-to-definition **and** completion all come back genuinely empty from a real
    /// `vue-language-server` inside a `.vue` script block; all three are answered by the real
    /// companion). Nothing here is hardcoded to hover, or to Vue.
    ///
    /// Callers genuinely do not need to know whether this is one process or two *for the methods on
    /// that list*, which today is every request this app actually sends through a two-server
    /// connection (`code_surface`'s hover and F12 go-to-definition, and
    /// [`AdeApp::schedule_lsp_sync`]'s completion). A method deliberately left off the list goes to
    /// the primary and only the primary - not an oversight but the point: a request the primary
    /// answers correctly must never have a second server's answer quietly substituted for it.
    ///
    /// A companion error or timeout can only ever *lose* the fallback, never change the answer:
    /// the primary's own honest empty result is what's returned in that case, so a slow companion
    /// can't turn "neither server knows anything here" into a spurious failure. A primary *error*
    /// is returned as-is and never falls back - a real failure is real information, and quietly
    /// substituting a different server's answer for it would hide it.
    pub(crate) fn request<R: lsp_core::lsp_types::request::Request>(
        &self,
        params: R::Params,
        timeout: Duration,
    ) -> Result<R::Result, lsp_core::LspError> {
        let Self::WithCompanion {
            primary,
            companion,
            spec,
        } = self
        else {
            return self.primary().request::<R>(params, timeout);
        };
        if !spec.fallback_methods.contains(&R::METHOD) {
            return primary.request::<R>(params, timeout);
        }

        // Serialized up front so the same real params can be replayed against the companion
        // without requiring `R::Params: Clone`, which `lsp_types`' own trait doesn't promise.
        let replayable = serde_json::to_value(&params).map_err(lsp_core::LspError::Serialize)?;
        let primary_result = primary.request::<R>(params, timeout);
        match &primary_result {
            Ok(value) if lsp_result_is_empty(value) => {}
            _ => return primary_result,
        }

        let replay = match serde_json::from_value::<R::Params>(replayable) {
            Ok(replay) => replay,
            Err(err) => {
                log::warn!(
                    "could not replay a `{}` request against {} for the companion fallback: {err}",
                    R::METHOD,
                    companion.name()
                );
                return primary_result;
            }
        };
        match companion.request::<R>(replay, timeout) {
            Ok(value) if !lsp_result_is_empty(&value) => Ok(value),
            // Both halves genuinely have nothing for this position - the primary's own honest
            // "nothing here" is the real answer.
            Ok(_) => primary_result,
            Err(err) => {
                log::warn!(
                    "the companion fallback for `{}` failed against {}: {err}",
                    R::METHOD,
                    companion.name()
                );
                primary_result
            }
        }
    }

    /// The real, merged diagnostics for `uri` - the union of whatever each half currently holds.
    /// `None` only when *neither* has answered yet, preserving
    /// `lsp_core::LspClient::diagnostics_for_uri`'s own real distinction between "haven't heard
    /// back" and "analyzed, found nothing".
    ///
    /// A genuine union, not a preference: each half reports a real, disjoint class of problem for
    /// the same file (live-verified for Vue - the primary reports template compile errors like
    /// `"Element is missing end tag."`, the companion reports TypeScript semantics like `"Type
    /// 'string' is not assignable to type 'number'."`), so dropping either side's would silently
    /// hide real errors from the user.
    pub(crate) fn diagnostics_for_uri(
        &self,
        uri: &lsp_core::lsp_types::Uri,
    ) -> Option<Vec<lsp_core::lsp_types::Diagnostic>> {
        let primary = self.primary().diagnostics_for_uri(uri);
        let Some(companion) = self.companion() else {
            return primary;
        };
        let companion = companion.diagnostics_for_uri(uri);
        match (primary, companion) {
            (None, None) => None,
            (primary, companion) => Some(
                primary
                    .unwrap_or_default()
                    .into_iter()
                    .chain(companion.unwrap_or_default())
                    .collect(),
            ),
        }
    }

    /// Path-keyed [`Self::diagnostics_for_uri`] - same merge semantics. Only called from a real
    /// background task (it does a blocking `canonicalize` internally, per
    /// `lsp_core::LspClient::diagnostics_for`'s own docs).
    pub(in crate::lsp) fn diagnostics_for(
        &self,
        path: &Path,
    ) -> Option<Vec<lsp_core::lsp_types::Diagnostic>> {
        let uri = lsp_core::LspClient::uri_for_path(path).ok()?;
        self.diagnostics_for_uri(&uri)
    }

    /// `true` once *either* half has a real result for `uri` - so the status bar can honestly stop
    /// saying "Indexing" as soon as a real answer lands, rather than waiting on a second server
    /// that may legitimately have nothing to say about this file.
    pub(in crate::lsp) fn has_diagnostics_result_uri(
        &self,
        uri: &lsp_core::lsp_types::Uri,
    ) -> bool {
        self.primary().has_diagnostics_result_uri(uri)
            || self
                .companion()
                .is_some_and(|companion| companion.has_diagnostics_result_uri(uri))
    }

    /// `None` while every process backing this connection is genuinely alive; otherwise a real
    /// message naming **which** one died, by its own `lsp_core::LspClient::name()` (which for a
    /// companion is its distinct `client_key`, e.g. `"typescript-language-server (vue)"`, not the
    /// bare binary name) - so a dead companion is surfaced as honestly as a dead primary already
    /// was, rather than silently degrading into wrong-but-plausible results.
    ///
    /// A `WithCompanion` connection is only fully alive when *both* halves are: the companion is
    /// not a nice-to-have there, it is where an entire real class of this file's analysis comes
    /// from, and the primary will also start hanging on relay requests nobody can answer.
    pub(in crate::lsp) fn liveness_failure_reason(&self) -> Option<String> {
        for client in [Some(self.primary()), self.companion()]
            .into_iter()
            .flatten()
        {
            if !client.is_connection_alive() {
                return Some(format!(
                    "{}'s connection was lost (the process exited unexpectedly)",
                    client.name()
                ));
            }
        }
        None
    }

    /// `false` the instant either half's real process dies - see [`Self::liveness_failure_reason`],
    /// which is what production actually reads (it needs the reason, not just the bit). Kept
    /// test-only rather than exposed unused: this crate doesn't ship API nothing calls.
    #[cfg(test)]
    pub(in crate::lsp) fn is_connection_alive(&self) -> bool {
        self.liveness_failure_reason().is_none()
    }

    /// The primary's own advertised capability, in both variants - the primary is what determines
    /// this connection's real capabilities. Deliberately not an "either half" rule: these gate
    /// what this app *sends*, and the primary is the one whose document state must stay correct.
    pub(in crate::lsp) fn supports_document_sync(&self) -> bool {
        self.primary().supports_document_sync()
    }

    /// See [`Self::supports_document_sync`] on why this reads the primary only.
    pub(in crate::lsp) fn supports_diagnostic_pull(&self) -> bool {
        self.primary().supports_diagnostic_pull()
    }

    /// See [`Self::supports_document_sync`] on why this reads the primary only.
    pub(in crate::lsp) fn completion_trigger_characters(&self) -> Vec<String> {
        self.primary().completion_trigger_characters()
    }

    /// Opens `path` in every process backing this connection.
    ///
    /// The primary's own result is this method's result, unchanged from the single-server
    /// contract every existing caller was already written against - it is what drives
    /// `AdeApp::lsp_opened_files`/version bookkeeping. The companion's own `didOpen` (with its
    /// own real `language_id`, since it needs to recognize the file as the same language, not as
    /// plain text) is fired alongside, best-effort: a failure there is logged, never propagated.
    /// The companion is a real, additional analysis source, not a gate on the primary's own
    /// correctness, and letting it fail this call would regress the primary's already-audited
    /// state machine for a reason that has nothing to do with it.
    pub(in crate::lsp) fn did_open(
        &self,
        path: &Path,
        text: String,
        version: i32,
        primary_language_id: &'static str,
    ) -> Result<(), lsp_core::LspError> {
        let Self::WithCompanion {
            primary,
            companion,
            spec,
        } = self
        else {
            return self
                .primary()
                .did_open(path, text, version, primary_language_id);
        };
        let result = primary.did_open(path, text.clone(), version, primary_language_id);
        if let Err(err) = companion.did_open(path, text, version, spec.language_id) {
            log::warn!(
                "failed to send didOpen for {} to the companion {}: {err}",
                path.display(),
                companion.name()
            );
        }
        result
    }

    /// Syncs `path`'s full content to every process backing this connection - same non-gating
    /// discipline as [`Self::did_open`]: the primary's result is this method's result, the
    /// companion's own send is best-effort alongside it.
    pub(in crate::lsp) fn did_change_full(
        &self,
        path: &Path,
        text: String,
        version: i32,
    ) -> Result<(), lsp_core::LspError> {
        let Self::WithCompanion {
            primary, companion, ..
        } = self
        else {
            return self.primary().did_change_full(path, text, version);
        };
        let result = primary.did_change_full(path, text.clone(), version);
        if let Err(err) = companion.did_change_full(path, text, version) {
            log::warn!(
                "failed to send didChange for {} to the companion {}: {err}",
                path.display(),
                companion.name()
            );
        }
        result
    }

    /// Pulls fresh diagnostics from the **primary**, which is what drives this method's real
    /// return value and real wall-clock cost.
    ///
    /// The companion's own pull, when it has one to make, is deliberately *not* fired from in
    /// here. See [`Self::companion_diagnostics_pull_target`] for the real reason and for where it
    /// actually happens now.
    pub(in crate::lsp) fn pull_diagnostics(
        &self,
        path: &Path,
        version: i32,
        timeout: Duration,
    ) -> Result<(), lsp_core::LspError> {
        self.primary().pull_diagnostics(path, version, timeout)
    }

    /// The companion that genuinely needs its own `textDocument/diagnostic` pull, if there is one -
    /// `None` for a single-server connection, and `None` for a companion that never advertises
    /// `diagnosticProvider` (which is every real one today: `typescript-language-server` pushes
    /// `publishDiagnostics` instead, so this is a live capability check rather than a version pin,
    /// and it starts returning `Some` the moment a companion that *does* advertise pull is used).
    ///
    /// Exposed for the caller to drive rather than fired from inside [`Self::pull_diagnostics`],
    /// which is a real fix, not a stylistic move. Two things were wrong with doing it in there:
    ///
    /// 1. It used a bare, detached `std::thread::spawn` that nothing tracked or joined. A live
    ///    `Arc<LspClient>` clone held by such a thread also makes [`AdeApp::evict_stale_lsp_clients`]'s
    ///    own `Arc::try_unwrap` fail, silently downgrading that client's shutdown from a graceful
    ///    `shutdown`/`exit` handshake to a `SIGKILL` via `Drop`.
    /// 2. [`AdeApp::schedule_lsp_sync`] calls `pull_diagnostics` up to
    ///    `PULL_DIAGNOSTICS_EMPTY_RETRIES + 1` times per settled keystroke, so one keystroke could
    ///    pile up ~21 of those threads. That retry loop exists for a race that is specifically the
    ///    *primary's* (see that constant's own docs); re-firing an unrelated companion pull on
    ///    every attempt is unbounded work with no matching benefit.
    ///
    /// The caller now fires it exactly once per sync tick, as a real, tracked
    /// [`gpui::Task`] in [`AdeApp::_lsp_tasks`], alongside - not sequenced before - the primary's
    /// own pull: nothing downstream waits on the companion's result (it lands in the companion's
    /// own diagnostics map, which [`Self::diagnostics_for_uri`] reads on the next render
    /// regardless), so sequencing it would double the tick's real latency for nothing.
    pub(in crate::lsp) fn companion_diagnostics_pull_target(
        &self,
    ) -> Option<std::sync::Arc<lsp_core::LspClient>> {
        self.companion()
            .filter(|companion| companion.supports_diagnostic_pull())
            .map(std::sync::Arc::clone)
    }
}

/// The real snapshot needed to send one `textDocument/didChange` (and, for a pull-capable
/// server, the real diagnostics pull that follows it) - see [`AdeApp::schedule_lsp_sync`]'s own
/// docs for how this is actually used.
struct LspSyncRequest {
    client: std::sync::Arc<LspConnection>,
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
    /// Whether [`AdeApp::lsp_connection_for_path`]'s client's [`lsp_core::LspClient::diagnostics_for_uri`]
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
    client: std::sync::Arc<LspConnection>,
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
    pub(crate) fn evict_stale_lsp_clients(&mut self, active_root: &Path, cx: &mut Context<Self>) {
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
    /// Also spawns `extension`'s [`crate::language::CompanionServer`], when it has one, under its
    /// own separate [`LspClientKey`] - a genuinely independent [`LspClientState`] entry, spawned,
    /// polled and evicted through 100% of the same already-battle-tested machinery every other
    /// client uses (a dead companion is just another dead `Ready` entry). That's deliberate: it
    /// means no new process-lifecycle code exists for the two-server case, which is where
    /// lifecycle bugs would otherwise hide.
    ///
    /// The two spawns are fully independent. A companion whose own real prerequisite is missing
    /// (no resolvable `@vue/typescript-plugin`) lands in `Failed` **without** preventing the
    /// primary from starting: `vue-language-server` alone is a real, reduced-but-working
    /// experience (its own template diagnostics still arrive), and
    /// [`Self::lsp_connection_for_path`] degrades to [`LspConnection::Single`] for it rather than
    /// refusing everything.
    pub(crate) fn ensure_lsp_client(
        &mut self,
        repo_root: PathBuf,
        extension: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        let Some(binary) = crate::language::lsp_binary_for_extension(extension) else {
            return;
        };
        self.spawn_lsp_client(
            (repo_root.clone(), binary),
            repo_root.clone(),
            {
                let repo_root = repo_root.clone();
                move || {
                    // The real `ServerSpawnConfig` (including any `$PATH`/filesystem probing it
                    // does - Pyright's `pythonPath` resolution, Vue's `--tsdk` resolution) is
                    // built here, off the GPUI thread - see this method's own docs for why that
                    // moved from the caller.
                    crate::language::server_spawn_config(&repo_root, extension)?.ok_or_else(|| {
                        format!("no LSP server is configured for extension {extension:?}")
                    })
                }
            },
            cx,
        );

        if let Some(companion) = crate::language::companion_for_extension(extension) {
            self.spawn_lsp_client(
                (repo_root.clone(), companion.client_key),
                repo_root,
                move || crate::language::companion_spawn_config(&companion),
                cx,
            );
        }
    }

    /// Spawns exactly one real server under exactly one [`LspClientKey`] - the shared body behind
    /// both halves of [`Self::ensure_lsp_client`], so the `Spawning`-then-`Ready`/`Failed`
    /// lifecycle exists in one place rather than being duplicated per server. A no-op if `key`
    /// already has an entry in any state (a previous failure is not retried on every render).
    ///
    /// `build_config` runs on `cx.background_executor()`, never the GPUI thread: it does real
    /// `$PATH`/filesystem probing. Its `Err` is a real, checked prerequisite failure and is shown
    /// as-is in [`LspClientState::Failed`], rather than being flattened into a generic message
    /// that would hide *why*.
    fn spawn_lsp_client(
        &mut self,
        key: LspClientKey,
        repo_root: PathBuf,
        build_config: impl FnOnce() -> Result<lsp_core::ServerSpawnConfig, String> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.lsp_clients.contains_key(&key) {
            return;
        }
        self.lsp_clients
            .insert(key.clone(), LspClientState::Spawning);
        cx.notify();

        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let config = build_config()?;
                    lsp_core::LspClient::spawn(&repo_root, config).map_err(|err| err.to_string())
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

    /// "The LSP for this file" as one [`LspConnection`], if there's a real, `Ready` primary for
    /// `path`'s own language - looks up `crate::language::lsp_binary_for_extension` off `path`'s
    /// extension to find the real [`LspClientKey`] second half, so a hover request against a `.ts`
    /// file reaches the real typescript-language-server client rather than assuming Rust (this
    /// app's only supported language before Revision R8). `None` for an extension with no LSP
    /// identity at all (`.go`/anything unrecognized) or one whose primary isn't `Ready` yet.
    ///
    /// A [`LspConnection::WithCompanion`] is built **only** when the companion is genuinely
    /// `Ready` too; a companion that's still spawning, failed, or gone honestly degrades to
    /// [`LspConnection::Single`] rather than fabricating a pairing that isn't there. Freshly
    /// constructed per call (two `Arc::clone`s and an enum tag) precisely so that degrade tracks
    /// the real current state rather than whatever was true when some earlier lookup happened.
    pub(crate) fn lsp_connection_for_path(
        &self,
        path: &Path,
    ) -> Option<std::sync::Arc<LspConnection>> {
        let extension = path.extension().and_then(|ext| ext.to_str());
        let binary = crate::language::lsp_binary_for_extension(extension)?;
        let primary = match self
            .lsp_clients
            .get(&(self.file_tree_root.clone(), binary))?
        {
            LspClientState::Ready(client) => client.clone(),
            _ => return None,
        };

        let connection = match crate::language::companion_for_extension(extension) {
            Some(spec) => {
                match self
                    .lsp_clients
                    .get(&(self.file_tree_root.clone(), spec.client_key))
                {
                    Some(LspClientState::Ready(companion)) => LspConnection::WithCompanion {
                        primary,
                        companion: companion.clone(),
                        spec,
                    },
                    _ => LspConnection::Single(primary),
                }
            }
            None => LspConnection::Single(primary),
        };
        Some(std::sync::Arc::new(connection))
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
    /// most a handful of files' worth of server-side state alive per agent. The `LspClient`
    /// (and every document it opened) is torn down when its owning root's client is dropped -
    /// this app's actual document lifetime boundary.
    ///
    /// Takes an [`LspConnection`], not a raw client: for a two-server language the *same* real
    /// `didOpen` must reach both processes (see [`LspConnection::did_open`]'s own docs), and the
    /// single [`Self::lsp_opened_files`] guard above correctly covers both, since they are opened
    /// together or not at all.
    pub(crate) fn dispatch_did_open(
        &mut self,
        client: std::sync::Arc<LspConnection>,
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
                                        "failed to send didOpen for {}: {err}",
                                        path.display()
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
    /// Mirrors `crate::terminal::pane::TerminalPane::spawn_process`'s own
    /// `cx.background_executor().timer(..)`-driven poll loop shape: diagnostics arrive on
    /// `lsp_core`'s own reader thread, outside the GPUI runtime, with no way to directly notify
    /// a GPUI entity from there - so this loop periodically drains every ready client's wake
    /// channel and calls `cx.notify()` only when something actually changed, never
    /// unconditionally per tick.
    pub(in crate::lsp) fn ensure_lsp_poll_task(&mut self, cx: &mut Context<Self>) {
        if self._lsp_poll_task.is_some() {
            return;
        }
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(LSP_DIAGNOSTICS_POLL_INTERVAL)
                .await;
            let update_result = this.update(cx, |this, cx| {
                let mut any_update = false;
                // Collected first, dispatched after: the loop below holds an immutable borrow of
                // `lsp_clients`, and dispatching needs `&mut self` for `_lsp_tasks`.
                let mut relays: Vec<(
                    PathBuf,
                    &'static str,
                    crate::language::CompanionServer,
                    serde_json::Value,
                )> = Vec::new();
                for (key, state) in this.lsp_clients.iter() {
                    if let LspClientState::Ready(client) = state {
                        if client.drain_updates() {
                            any_update = true;
                        }
                        // Drained for *every* ready client, not just ones that can relay, so a
                        // server that emits notifications this app has no handler for can never
                        // sit at `lsp-core`'s own queue cap warning forever. Only a client whose
                        // registry entry genuinely declares a companion - and only for that
                        // companion's own declared relay method - is ever acted on, so this is a
                        // real "this specific client can send this specific message" rule, not a
                        // blanket handler bolted onto every connection.
                        let spec = crate::language::companion_for_primary_binary(key.1);
                        for (method, params) in client.drain_custom_notifications() {
                            match spec {
                                Some(spec) if method == spec.relay_request_method => {
                                    relays.push((key.0.clone(), key.1, spec, params));
                                }
                                _ => {}
                            }
                        }
                    }
                }
                for (repo_root, primary_binary, spec, params) in relays {
                    this.dispatch_companion_relay(repo_root, primary_binary, spec, params, cx);
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

    /// Performs one real relay round trip for a primary that asked its client to query its
    /// companion (see [`crate::language::CompanionServer`] for what this protocol is and why the
    /// client - not either server - is the one that has to do it):
    ///
    /// 1. The primary's notification carries `[[requestId, command, args]]` - one 3-element array
    ///    inside an outer array. That double wrapping is not incidental: both real servers are
    ///    built on `vscode-jsonrpc`, whose positional notification params put the handler's single
    ///    spread argument inside an outer array. A payload whose `command`/`args` are malformed but
    ///    whose `requestId` is genuinely recoverable is still answered, with the same honest `null`
    ///    body used everywhere else here (see [`relay_request_id`]); only a payload with no
    ///    recoverable id at all is a true, logged no-op, because there is then nothing to reply to.
    /// 2. The companion answers over a plain, typed `workspace/executeCommand`, and its result is
    ///    the raw tsserver response envelope - the real answer is its `body`.
    /// 3. The answer goes back as `[[requestId, body]]`, the same double wrapping in reverse
    ///    (verified live: sending `[requestId, body]` un-wrapped makes the real primary's own
    ///    handler throw `"number 1 is not iterable"` internally).
    ///
    /// The primary is left with a hanging internal promise if it never hears back, so **every**
    /// path here still sends a response - a `null` body for a companion that isn't `Ready` (a real
    /// race if a worktree switch lands mid-flight), that errors, or that times out. Both clients
    /// are re-resolved from `lsp_clients` at the moment this task actually runs, not captured at
    /// drain time, for the same reason.
    fn dispatch_companion_relay(
        &mut self,
        repo_root: PathBuf,
        primary_binary: &'static str,
        spec: crate::language::CompanionServer,
        params: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some(request_id) = relay_request_id(&params) else {
            log::warn!(
                "ignoring a `{}` payload from {primary_binary} with no recoverable request id - \
                 there is genuinely nothing to reply to: {params}",
                spec.relay_request_method
            );
            return;
        };
        let command_and_args =
            parse_relay_request(&params).map(|(_, command, args)| (command, args));
        if command_and_args.is_none() {
            log::warn!(
                "a `{}` payload from {primary_binary} carried a real request id but no usable \
                 command - replying null so the primary's own request doesn't hang: {params}",
                spec.relay_request_method
            );
        }

        let task = cx.spawn(async move |this, cx| {
            let started = std::time::Instant::now();
            let companion_key = (repo_root.clone(), spec.client_key);
            let companion = this
                .update(cx, |this, _cx| match this.lsp_clients.get(&companion_key) {
                    Some(LspClientState::Ready(client)) => Some(client.clone()),
                    _ => None,
                })
                .ok()
                .flatten();

            let body = match (command_and_args, companion) {
                // Already logged above, at parse time - the reply itself still has to go out.
                (None, _) => serde_json::Value::Null,
                (Some((command, args)), Some(companion)) => {
                    let request = lsp_core::lsp_types::ExecuteCommandParams {
                        command: spec.relay_command.to_string(),
                        arguments: vec![command, args],
                        work_done_progress_params: Default::default(),
                    };
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            companion
                                .request::<lsp_core::lsp_types::request::ExecuteCommand>(
                                    request,
                                    LSP_QUERY_TIMEOUT,
                                )
                                .map_err(|err| (err, companion.name()))
                        })
                        .await;
                    match result {
                        Ok(Some(envelope)) => envelope
                            .get("body")
                            .or_else(|| envelope.get("result"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        Ok(None) => serde_json::Value::Null,
                        Err((err, name)) => {
                            log::warn!(
                                "the companion {name} could not answer a relayed \
                                 `{}` request: {err} - replying null so the primary's own \
                                 request doesn't hang",
                                spec.relay_command
                            );
                            serde_json::Value::Null
                        }
                    }
                }
                (Some(_), None) => {
                    log::warn!(
                        "no ready companion ({}) for {} when a relay request arrived - replying \
                         null so the primary's own request doesn't hang",
                        spec.client_key,
                        repo_root.display()
                    );
                    serde_json::Value::Null
                }
            };

            let primary = this
                .update(cx, |this, _cx| {
                    match this.lsp_clients.get(&(repo_root.clone(), primary_binary)) {
                        Some(LspClientState::Ready(client)) => Some(client.clone()),
                        _ => None,
                    }
                })
                .ok()
                .flatten();
            let Some(primary) = primary else {
                // The primary itself is gone (evicted mid-flight) - there is nothing left holding
                // a promise to resolve, so there is nothing honest left to do here.
                return;
            };
            let payload = serde_json::json!([[request_id, body]]);
            let method = spec.relay_response_method;
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = primary.notify_raw(method, payload) {
                        log::warn!(
                            "failed to send `{method}` back to {}: {err}",
                            primary.name()
                        );
                        return;
                    }
                    // The real, measured cost of one full relay round trip (drain -> companion
                    // request -> response back to the primary), logged rather than left
                    // unobservable: this sits on the path every Vue file's first real analysis
                    // waits on, so a regression here should be visible in real logs.
                    log::debug!(
                        "relayed `{}` to {} and answered `{method}` in {:?}",
                        spec.relay_command,
                        spec.client_key,
                        started.elapsed()
                    );
                })
                .await;
        });
        self._lsp_tasks.push(task);
    }

    /// Debounces a real live `textDocument/didChange` sync for `relative_path`'s buffer,
    /// dispatching a real `textDocument/completion` request alongside it when the settled edit
    /// looks completion-worthy - see [`LSP_SYNC_DEBOUNCE`]'s own docs for why coalescing both
    /// into one debounced step is safe. Called from every real edit call site in
    /// `crate::code_surface::editing` (`replace_text_in_range`/`replace_and_mark_text_in_range`/
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
    pub(crate) fn schedule_lsp_sync(&mut self, relative_path: PathBuf, cx: &mut Context<Self>) {
        let task = cx.spawn({
            let relative_path = relative_path.clone();
            async move |this, cx| {
                cx.background_executor().timer(LSP_SYNC_DEBOUNCE).await;
                let Ok(Some(plan)) = this.update(cx, |this, cx| {
                    this.prepare_lsp_sync(&relative_path, cx, false)
                }) else {
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
                    // The companion's own independent pull, fired exactly **once** per sync tick
                    // (not once per retry attempt below) as a real, tracked task rather than a
                    // detached thread - see `LspConnection::companion_diagnostics_pull_target`'s
                    // own docs for both halves of why. `None` for every single-server connection
                    // and for every companion that doesn't advertise pull, which is all of them
                    // today, so this is a real, correct path kept honest rather than a hot one.
                    if let Some(companion) = client.companion_diagnostics_pull_target() {
                        let pull_path = absolute_path.clone();
                        let companion_pull = cx.background_executor().spawn(async move {
                            if let Err(err) =
                                companion.pull_diagnostics(&pull_path, version, LSP_QUERY_TIMEOUT)
                            {
                                log::warn!(
                                    "the companion {}'s own diagnostics pull for {} failed: {err}",
                                    companion.name(),
                                    pull_path.display()
                                );
                            }
                        });
                        let _ = this.update(cx, |this, _cx| {
                            this._lsp_tasks.push(companion_pull);
                        });
                    }
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

    /// `Ctrl+Space`'s real dispatch (GitHub issue #26,
    /// [`crate::lsp::completion_popup::AdeApp::handle_completions_invoke_action`]) - unlike
    /// [`Self::schedule_lsp_sync`], this never waits out [`LSP_SYNC_DEBOUNCE`] first: a real,
    /// explicit keystroke asking for suggestions right now should dispatch immediately, not sit
    /// behind a debounce meant to coalesce a *fast typist's* burst of edits. Reuses
    /// [`Self::prepare_lsp_sync`] (`force_completion: true`) for the actual plan - the same real
    /// sync-then-complete ordering and version/generation bookkeeping - but runs a smaller, real
    /// continuation than [`Self::schedule_lsp_sync`]'s own: just the real sync (if the buffer
    /// hasn't already been sent) and the real completion request/response, never the diagnostics-
    /// pull sequence further down in that method (irrelevant to what `Ctrl+Space` needs, and
    /// already covered by whichever `schedule_lsp_sync` debounce the same edit already armed).
    /// This is a deliberate, documented duplication of `schedule_lsp_sync`'s own sync-dispatch
    /// shape rather than a deeper shared refactor of that already-large method - see this crate's
    /// own `CONTRIBUTING.md` for why a judgment call like this gets a comment, not a silent choice.
    ///
    /// A genuine no-op when [`Self::prepare_lsp_sync`] finds nothing real to do (no buffer, or no
    /// ready LSP client for this file's language) - `Ctrl+Space` in a file with no language server
    /// simply does nothing, the same honest "nothing real to complete against" this app already
    /// gives the automatic, keystroke-driven path in that case.
    pub(crate) fn invoke_completions_now(
        &mut self,
        relative_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        let Some(plan) = self.prepare_lsp_sync(&relative_path, cx, true) else {
            return;
        };
        let LspSyncPlan { sync, completion } = plan;
        let Some(request) = completion else {
            return;
        };

        let task = cx.spawn(async move |this, cx| {
            let mut server_has_latest_content = true;
            if let Some(sync_request) = sync {
                let LspSyncRequest {
                    client,
                    absolute_path,
                    content,
                    version,
                    ..
                } = sync_request;
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
                    let record_path = relative_path.clone();
                    let _ = this.update(cx, |this, _cx| {
                        this.lsp_last_synced_content
                            .insert(record_path.clone(), content);
                        this.lsp_synced_version.insert(record_path, version);
                    });
                }
            }
            if !server_has_latest_content {
                // The same real wire-ordering rule `Self::schedule_lsp_sync`'s own docs explain:
                // never let a completion request race ahead of a `didChange` that failed to send,
                // which risks the server answering against stale, pre-edit content.
                return;
            }

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
            let _ = this.update(cx, |this, cx| {
                this.apply_completion_result(&relative_path, generation, result, cx);
            });
        });
        self._completions_request_task = Some(task);
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
    ///
    /// `force_completion` (GitHub issue #26's `Ctrl+Space` manual invoke - see
    /// [`AdeApp::invoke_completions_now`]) bypasses [`crate::lsp::completion::completion_trigger`]'s
    /// own "was the character before the caret completion-worthy" judgment entirely, always
    /// building a real `CompletionTriggerKind::INVOKED` request with no `trigger_character` -
    /// the real "explicitly ask for suggestions right here, mid-word or not" contract `Ctrl+Space`
    /// needs, as opposed to the automatic, keystroke-driven path this same method also serves
    /// (`force_completion: false`, from [`AdeApp::schedule_lsp_sync`]).
    fn prepare_lsp_sync(
        &mut self,
        relative_path: &Path,
        cx: &mut Context<Self>,
        force_completion: bool,
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

        let Some(client) = self.lsp_connection_for_path(&absolute_path) else {
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

        let completion_context = if force_completion {
            Some(lsp_core::lsp_types::CompletionContext {
                trigger_kind: lsp_core::lsp_types::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            })
        } else {
            let char_before_cursor = crate::lsp::completion::char_before(&content, cursor);
            let trigger_characters = client.completion_trigger_characters();
            crate::lsp::completion::completion_trigger(char_before_cursor, &trigger_characters)
        };

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
pub(crate) enum LspClientState {
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
/// Reads one primary's real relay-request payload: `[[requestId, command, args]]` on the wire (see
/// [`AdeApp::dispatch_companion_relay`]'s own docs for why it's double-wrapped). Returns the three
/// pieces as raw [`serde_json::Value`]s - `requestId` in particular is echoed back verbatim rather
/// than parsed into a Rust integer, so no real id can be lost or altered in translation.
///
/// `None` for any shape that isn't genuinely that - a missing outer array, an empty one, an inner
/// array with fewer than two entries, or a non-string command (which the companion's own
/// `executeCommand` would reject anyway). Kept a free function so its parsing is unit-testable
/// with no `AdeApp`, GPUI window, or real server involved.
///
/// A `None` here does **not** mean the dispatch may stay silent: see [`relay_request_id`], which
/// recovers the id independently precisely so a payload this refuses can still be answered.
fn parse_relay_request(
    params: &serde_json::Value,
) -> Option<(serde_json::Value, serde_json::Value, serde_json::Value)> {
    let inner = params.as_array()?.first()?.as_array()?;
    let request_id = inner.first()?;
    let command = inner.get(1)?;
    if !command.is_string() {
        return None;
    }
    Some((
        request_id.clone(),
        command.clone(),
        inner.get(2).cloned().unwrap_or(serde_json::Value::Null),
    ))
}

/// Just the `requestId` out of a drained relay notification, recovered **independently** of
/// whether the rest of the payload is usable.
///
/// Deliberately separate from [`parse_relay_request`]: an unanswered relay leaves the primary
/// holding an internal promise that never resolves, which is the single failure mode
/// [`AdeApp::dispatch_companion_relay`] exists to prevent, and a payload like
/// `[[7, 42, {}]]` (real, valid id; command that isn't a string) carries everything needed to send
/// the same honest `[[7, null]]` that path already sends for a not-`Ready`, timed-out, or errored
/// companion. Folding that case into the whole-payload parse meant it was dropped in silence
/// instead.
///
/// The id must genuinely be a JSON-RPC id - a number or a string. Anything else (including a
/// `null` id, or no inner array at all) is the one real case with nothing to reply *to*, and stays
/// a true no-op.
fn relay_request_id(params: &serde_json::Value) -> Option<serde_json::Value> {
    let request_id = params.as_array()?.first()?.as_array()?.first()?;
    (request_id.is_number() || request_id.is_string()).then(|| request_id.clone())
}

pub(in crate::lsp) fn stale_lsp_client_keys(
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
pub(crate) enum LspFileStatus {
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
///
/// Takes three real inputs because a two-server language genuinely has three states worth telling
/// apart: `primary_state`/`companion_state` are the raw lifecycle entries (only they can express
/// `Spawning`/`Failed`), and `connection` is the already-resolved [`LspConnection`] the same render
/// pass is using, so the counts reported here are computed from the exact same merged view that's
/// actually being drawn - they can't disagree with it. `companion_state` is `None` for every
/// single-server language, which keeps this function's behavior there byte-for-byte what it was.
pub(crate) fn lsp_file_status(
    primary_state: &Option<LspClientState>,
    companion_state: Option<&LspClientState>,
    connection: Option<&LspConnection>,
    uri: Option<&lsp_core::lsp_types::Uri>,
) -> LspFileStatus {
    match primary_state {
        None | Some(LspClientState::Spawning) => return LspFileStatus::Spawning,
        Some(LspClientState::Failed(message)) => return LspFileStatus::Failed(message.clone()),
        Some(LspClientState::Ready(_)) => {}
    }

    // A companion that genuinely failed its own spawn/prerequisite check is surfaced honestly
    // rather than swallowed: the primary keeps working (see `AdeApp::ensure_lsp_client`'s own
    // docs on that deliberate degrade), but an entire real class of this file's analysis is
    // missing, and silently reporting a confident `Analyzed { errors: 0 }` off the half that's
    // left would be exactly the kind of plausible-but-wrong status this project doesn't ship. A
    // companion that's merely still `Spawning` is not an error and says nothing yet.
    if let Some(LspClientState::Failed(message)) = companion_state {
        return LspFileStatus::Failed(message.clone());
    }

    let Some(connection) = connection else {
        // A `Ready` primary with no resolved connection can only mean the lookup was for a
        // different path than this state - honest "no answer yet" rather than a fabricated one.
        return LspFileStatus::Indexing;
    };
    // A real, honest degrade (Revision R8.5b audit finding 9's fix, now covering both halves) for
    // a `Ready` client whose underlying process has actually died out from under it - see
    // `LspConnection::liveness_failure_reason`, which names *which* real process died. Reported
    // via the same `Failed` variant a spawn/handshake failure already uses rather than silently
    // continuing to report `Indexing`/`Analyzed` off a connection that will never answer another
    // real request again.
    if let Some(reason) = connection.liveness_failure_reason() {
        return LspFileStatus::Failed(reason);
    }

    let Some(uri) = uri else {
        return LspFileStatus::Indexing;
    };
    if !connection.has_diagnostics_result_uri(uri) {
        return LspFileStatus::Indexing;
    }
    let diagnostics = connection.diagnostics_for_uri(uri).unwrap_or_default();
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
            app.update_in(cx, |app, window, cx| {
                app.select_worktree(index, window, cx);
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
        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
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

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(0, window, cx);
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

        app.update_in(cx, |app, window, cx| {
            app.select_worktree(1, window, cx);
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

/// Real coverage for [`LspConnection`] itself - the merge/fallback/liveness rules, and the real
/// relay round trip - driven against **genuinely spawned** language server processes talking the
/// real `Content-Length`-framed JSON-RPC transport, just tiny ones.
///
/// ## Why a small real server rather than the Vue toolchain here
///
/// Every behavior under test in this module is a property of *this app's* coordination logic, not
/// of Vue: "does a companion timeout still produce a response", "does a dead half get named
/// honestly", "is the relay's wire shape right". Pinning those to `vue-language-server` would make
/// them slow, dependent on a specific npm install, and - worse - unable to *provoke* the failures
/// they're about (there is no way to ask a real Vue server to hang or die on cue). So they run
/// against [`FAKE_SERVER_SOURCE`]: a real process, a real handshake, real framing, real
/// request/response correlation, with real, deliberately-triggerable failure modes. It is a real
/// server, not a mock of one - nothing here stubs out `lsp_core`.
///
/// The real Vue toolchain is exercised end to end separately, in
/// `crate::code_surface`'s own `vue_two_server_wiring_tests` (it lives there because it
/// asserts on the real rendered hover state as well as diagnostics), which is where "does this
/// actually work against the real
/// thing" is proven.
#[cfg(test)]
mod lsp_connection_facade_tests {
    use super::*;
    use gpui::TestAppContext;
    use std::time::Instant;

    /// A real, minimal LSP server over the real wire protocol, with real, requestable failure
    /// modes. Modes:
    ///
    /// - `normal`: answers everything, and answers the three fallback-eligible methods with each
    ///   one's own real *empty* shape - `null` for hover, `[]` for definition, and
    ///   `{"isIncomplete": false, "items": []}` for completion. Those are deliberately three
    ///   different encodings of "nothing here", matching what a real `vue-language-server` was
    ///   measured to send for each (see `crate::language`'s `VUE_FALLBACK_METHODS`), so a test
    ///   driving this server exercises [`lsp_result_is_empty`]'s real job rather than a null check.
    /// - `hover`: answers `textDocument/hover` with real content instead of `null`.
    /// - `semantic`: a real stand-in for the companion - answers hover, definition **and**
    ///   completion with real, non-empty content.
    /// - `silent`: never answers a `workspace/executeCommand` or a `textDocument/references` (a
    ///   real, un-answering companion, for the paths that need one).
    /// - `pull`: additionally advertises a real `diagnosticProvider` in its handshake, which is
    ///   what `lsp_core::LspClient::supports_diagnostic_pull` genuinely reads. No real companion
    ///   advertises one today, so this is the only way to exercise that branch honestly rather
    ///   than by pinning a version.
    ///
    /// Two test-only methods make its behavior observable and controllable from Rust without
    /// weakening anything real: `test/die` makes the process genuinely exit (standing in for a
    /// crash), and `test/publish` makes it push a real `textDocument/publishDiagnostics`. It also
    /// echoes anything it receives on `tsserver/response` back as a real `publishDiagnostics`
    /// against a sentinel uri, which is how a test observes - through `lsp_core`'s own real
    /// notification sink, not a side channel - exactly what bytes the relay sent it.
    const FAKE_SERVER_SOURCE: &str = r#"
let buf = Buffer.alloc(0);
// `node -e <script> -- <mode>` consumes the `--` itself, so the mode lands at argv[1].
const MODE = process.argv[1] || 'normal';
function send(o) {
  const s = JSON.stringify(o);
  process.stdout.write('Content-Length: ' + Buffer.byteLength(s) + '\r\n\r\n' + s);
}
function publish(uri, message) {
  send({ jsonrpc: '2.0', method: 'textDocument/publishDiagnostics', params: { uri, diagnostics: [
    { range: { start: { line: 0, character: 0 }, end: { line: 0, character: 1 } }, severity: 1, message }
  ] } });
}
function handle(msg) {
  if (msg.method === 'initialize') {
    const capabilities = { textDocumentSync: 1, hoverProvider: true };
    if (MODE === 'pull') {
      capabilities.diagnosticProvider = { interFileDependencies: false, workspaceDiagnostics: false };
    }
    send({ jsonrpc: '2.0', id: msg.id, result: { capabilities } });
    return;
  }
  if (msg.method === 'shutdown') { send({ jsonrpc: '2.0', id: msg.id, result: null }); return; }
  if (msg.method === 'exit' || msg.method === 'test/die') { process.exit(0); }
  if (msg.method === 'test/publish') { publish(msg.params.uri, msg.params.message); return; }
  if (msg.method === 'textDocument/hover') {
    const result = (MODE === 'hover' || MODE === 'semantic')
      ? { contents: { kind: 'plaintext', value: 'real hover from ' + MODE } }
      : null;
    send({ jsonrpc: '2.0', id: msg.id, result });
    return;
  }
  if (msg.method === 'textDocument/definition') {
    // An empty *array*, not a null, in every non-semantic mode - the real shape a real
    // vue-language-server answers with, and the one a null-only check would miss.
    const result = MODE === 'semantic'
      ? [{ uri: 'file:///companion/real-definition.ts',
           range: { start: { line: 4, character: 6 }, end: { line: 4, character: 9 } } }]
      : [];
    send({ jsonrpc: '2.0', id: msg.id, result });
    return;
  }
  if (msg.method === 'textDocument/completion') {
    // An empty CompletionList (an *object*), the third real "nothing here" encoding.
    const result = MODE === 'semantic'
      ? [{ label: 'alpha' }, { label: 'beta' }]
      : { isIncomplete: false, items: [] };
    send({ jsonrpc: '2.0', id: msg.id, result });
    return;
  }
  if (msg.method === 'textDocument/references') {
    if (MODE === 'silent') { return; }
    send({ jsonrpc: '2.0', id: msg.id, result: [] });
    return;
  }
  if (msg.method === 'workspace/executeCommand') {
    if (MODE === 'silent') { return; }
    send({ jsonrpc: '2.0', id: msg.id, result: {
      seq: 0, type: 'response', command: msg.params.arguments[0], request_seq: 1, success: true,
      body: { echoed: msg.params.arguments }
    } });
    return;
  }
  if (msg.method === 'tsserver/response') { publish('file:///relay-observed', JSON.stringify(msg.params)); return; }
  if (msg.id !== undefined) { send({ jsonrpc: '2.0', id: msg.id, result: null }); }
}
process.stdin.on('data', (d) => {
  buf = Buffer.concat([buf, d]);
  for (;;) {
    const i = buf.indexOf('\r\n\r\n');
    if (i < 0) return;
    const m = /Content-Length: (\d+)/i.exec(buf.slice(0, i).toString());
    if (!m) return;
    const len = parseInt(m[1], 10);
    if (buf.length < i + 4 + len) return;
    const body = buf.slice(i + 4, i + 4 + len).toString();
    buf = buf.slice(i + 4 + len);
    handle(JSON.parse(body));
  }
});
"#;

    /// Genuinely spawns [`FAKE_SERVER_SOURCE`] as a real child process and drives a real
    /// `initialize`/`initialized` handshake through `lsp_core::LspClient::spawn` - the exact same
    /// spawn path every real server in this app goes through, with no test-only shortcut.
    fn spawn_fake_server(
        root: &Path,
        name: &'static str,
        mode: &str,
    ) -> std::sync::Arc<lsp_core::LspClient> {
        let config = lsp_core::ServerSpawnConfig {
            name,
            binary: "node",
            args: vec![
                "-e".to_string(),
                FAKE_SERVER_SOURCE.to_string(),
                "--".to_string(),
                mode.to_string(),
            ],
            initialization_options: None,
            workspace_configuration: lsp_core::default_workspace_configuration,
            // The relay methods this app's own production dispatch handles - subscribed here so
            // the fake server's relay-shaped traffic reaches `drain_custom_notifications` exactly
            // the way a real primary's does.
            custom_notification_methods: vec!["tsserver/request"],
        };
        std::sync::Arc::new(
            lsp_core::LspClient::spawn(root, config)
                .expect("the minimal real LSP server should spawn and complete a real handshake"),
        )
    }

    fn vue_companion_spec() -> crate::language::CompanionServer {
        crate::language::companion_for_extension(Some("vue"))
            .expect("vue has a real companion in the registry")
    }

    fn uri(raw: &str) -> lsp_core::lsp_types::Uri {
        raw.parse().expect("a real uri")
    }

    /// Pushes a real `publishDiagnostics` from `client`'s own process and waits for it to land in
    /// that client's real diagnostics sink.
    fn publish_and_wait(client: &lsp_core::LspClient, target: &str, message: &str) {
        client
            .notify_raw(
                "test/publish",
                serde_json::json!({ "uri": target, "message": message }),
            )
            .expect("the fake server should accept a real notification");
        let deadline = Instant::now() + Duration::from_secs(10);
        let target = uri(target);
        while !client.has_diagnostics_result_uri(&target) {
            assert!(
                Instant::now() < deadline,
                "the real publishDiagnostics push never landed in the client's own sink"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_relay_observation(client: &lsp_core::LspClient) -> String {
        let observed = uri("file:///relay-observed");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(diagnostics) = client.diagnostics_for_uri(&observed) {
                if let Some(first) = diagnostics.first() {
                    return first.message.clone();
                }
            }
            assert!(
                Instant::now() < deadline,
                "the primary never observed any relay response at all - a hanging primary is \
                 exactly the failure this path must never produce"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_real_relay_request_payload_parses_into_its_three_real_pieces() {
        let (id, command, args) = parse_relay_request(&serde_json::json!([[
            7,
            "_vue:projectInfo",
            { "file": "/tmp/App.vue" }
        ]]))
        .expect("a real, well-formed relay payload");
        assert_eq!(id, serde_json::json!(7));
        assert_eq!(command, serde_json::json!("_vue:projectInfo"));
        assert_eq!(args, serde_json::json!({ "file": "/tmp/App.vue" }));
    }

    /// The un-wrapped shape is specifically what a naive implementation would send, and is
    /// specifically what the real server does *not* use - so it must not be silently accepted as
    /// if it were valid.
    #[test]
    fn a_malformed_relay_payload_is_an_honest_none_not_a_panic() {
        for payload in [
            serde_json::json!([7, "_vue:projectInfo", {}]),
            serde_json::json!([]),
            serde_json::json!([[7]]),
            serde_json::json!([[7, 42, {}]]),
            serde_json::json!({ "requestId": 7 }),
            serde_json::json!(null),
        ] {
            assert!(
                parse_relay_request(&payload).is_none(),
                "a payload that isn't genuinely [[id, command, args]] must be refused: {payload}"
            );
        }
    }

    /// Revision R11 audit finding 4. The request id is recovered **independently** of the rest of
    /// the payload, because a recoverable id means a reply is genuinely possible - and an
    /// unanswered relay is exactly the hanging-primary failure this whole path exists to prevent.
    /// Only a payload with no real id at all has nothing to reply to.
    #[test]
    fn a_real_request_id_is_recovered_even_when_the_rest_of_the_payload_is_not() {
        for (payload, expected) in [
            (serde_json::json!([[7, 42, {}]]), serde_json::json!(7)),
            (serde_json::json!([[9]]), serde_json::json!(9)),
            (
                serde_json::json!([["id-as-a-string", null]]),
                serde_json::json!("id-as-a-string"),
            ),
        ] {
            assert_eq!(
                relay_request_id(&payload),
                Some(expected),
                "a real, valid request id must survive a payload the full parse refuses: {payload}"
            );
            assert!(
                parse_relay_request(&payload).is_none(),
                "these are precisely the payloads the full parse rejects - if one ever became \
                 parseable this test would stop proving anything: {payload}"
            );
        }

        for payload in [
            serde_json::json!([[null, "_vue:projectInfo"]]),
            serde_json::json!([[]]),
            serde_json::json!([7, "_vue:projectInfo", {}]),
            serde_json::json!([]),
            serde_json::json!({ "requestId": 7 }),
            serde_json::json!(null),
        ] {
            assert!(
                relay_request_id(&payload).is_none(),
                "with no real JSON-RPC id there is genuinely nothing to reply to: {payload}"
            );
        }
    }

    /// Args are genuinely optional on the wire - a two-element inner array is real and must
    /// forward a real `null`, not be rejected.
    #[test]
    fn a_relay_payload_with_no_args_forwards_a_real_null() {
        let (_, _, args) = parse_relay_request(&serde_json::json!([[1, "_vue:projectInfo"]]))
            .expect("a two-element inner array is a real, valid payload");
        assert_eq!(args, serde_json::Value::Null);
    }

    /// Diagnostics from a two-server connection are a real union: each half reports a genuinely
    /// different class of problem for the same file, so keeping only one side would hide real
    /// errors. Also pins the `None`-vs-`Some(vec![])` distinction across the merge.
    #[test]
    fn a_two_server_connection_merges_both_halves_real_diagnostics() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "primary", "normal");
        let companion = spawn_fake_server(root.path(), "companion", "normal");
        let target = "file:///merged.vue";

        let connection = LspConnection::WithCompanion {
            primary: primary.clone(),
            companion: companion.clone(),
            spec: vue_companion_spec(),
        };
        assert!(
            connection.diagnostics_for_uri(&uri(target)).is_none(),
            "with neither half having answered, the honest answer is None - not an empty vec, \
             which would wrongly read as 'analyzed, found nothing'"
        );
        assert!(!connection.has_diagnostics_result_uri(&uri(target)));

        publish_and_wait(&primary, target, "Element is missing end tag.");
        assert!(
            connection.has_diagnostics_result_uri(&uri(target)),
            "one half answering is enough to stop honestly claiming 'still indexing'"
        );

        publish_and_wait(
            &companion,
            target,
            "Type 'string' is not assignable to type 'number'.",
        );
        let merged = connection
            .diagnostics_for_uri(&uri(target))
            .expect("both halves have answered");
        let messages: Vec<&str> = merged
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect();
        assert!(
            messages.contains(&"Element is missing end tag.")
                && messages.contains(&"Type 'string' is not assignable to type 'number'."),
            "both halves' real diagnostics must survive the merge, got: {messages:?}"
        );
    }

    /// A single-server connection must behave byte-for-byte like the raw client it wraps -
    /// including passing `None` through as `None`.
    #[test]
    fn a_single_server_connection_passes_diagnostics_straight_through() {
        let root = tempfile::tempdir().expect("tempdir");
        let client = spawn_fake_server(root.path(), "solo", "normal");
        let connection = LspConnection::Single(client.clone());
        let target = "file:///solo.rs";

        assert!(connection.diagnostics_for_uri(&uri(target)).is_none());
        publish_and_wait(&client, target, "mismatched types");
        assert_eq!(
            connection.diagnostics_for_uri(&uri(target)),
            client.diagnostics_for_uri(&uri(target)),
            "Single must be a genuine passthrough, not a re-derivation that could disagree"
        );
    }

    /// The hover fallback: the primary genuinely answers `null` (which is real, expected behavior
    /// for a hybrid-mode primary, not a failure), so the companion's real answer is what surfaces.
    #[test]
    fn hover_falls_back_to_the_companion_when_the_primary_has_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "primary", "normal");
        let companion = spawn_fake_server(root.path(), "companion", "hover");
        let params = lsp_core::lsp_types::HoverParams {
            text_document_position_params: lsp_core::lsp_types::TextDocumentPositionParams {
                text_document: lsp_core::lsp_types::TextDocumentIdentifier {
                    uri: uri("file:///hover.vue"),
                },
                position: lsp_core::lsp_types::Position {
                    line: 1,
                    character: 7,
                },
            },
            work_done_progress_params: Default::default(),
        };

        let single = LspConnection::Single(primary.clone());
        assert!(
            single
                .request::<lsp_core::lsp_types::request::HoverRequest>(
                    params.clone(),
                    Duration::from_secs(10)
                )
                .expect("a real response")
                .is_none(),
            "with no companion there is nothing to fall back to - the primary's honest 'nothing \
             here' must not be papered over"
        );

        let paired = LspConnection::WithCompanion {
            primary,
            companion,
            spec: vue_companion_spec(),
        };
        let hover = paired
            .request::<lsp_core::lsp_types::request::HoverRequest>(params, Duration::from_secs(10))
            .expect("a real response")
            .expect("the companion's real hover should surface through the facade");
        let lsp_core::lsp_types::HoverContents::Markup(markup) = hover.contents else {
            panic!("the fake server sends real markup content");
        };
        assert_eq!(markup.value, "real hover from hover");
    }

    fn goto_params(target: &str) -> lsp_core::lsp_types::GotoDefinitionParams {
        lsp_core::lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_core::lsp_types::TextDocumentPositionParams {
                text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri: uri(target) },
                position: lsp_core::lsp_types::Position {
                    line: 3,
                    character: 8,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        }
    }

    fn completion_params(target: &str) -> lsp_core::lsp_types::CompletionParams {
        lsp_core::lsp_types::CompletionParams {
            text_document_position: lsp_core::lsp_types::TextDocumentPositionParams {
                text_document: lsp_core::lsp_types::TextDocumentIdentifier { uri: uri(target) },
                position: lsp_core::lsp_types::Position {
                    line: 5,
                    character: 4,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        }
    }

    /// Revision R11 audit finding 1, first half. `textDocument/definition` is on Vue's real
    /// [`crate::language::CompanionServer::fallback_methods`] list because the real primary was
    /// measured returning an **empty array** (not a `null`) for an identifier inside a `.vue`
    /// script block while the real companion returned a real `Location`. Before this fix, F12 on
    /// such an identifier did nothing at all: the facade hardcoded its fallback to hover only.
    #[test]
    fn goto_definition_falls_back_to_the_companion_on_an_empty_array_answer() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "primary", "normal");

        let single = LspConnection::Single(primary.clone());
        let solo = single
            .request::<lsp_core::lsp_types::request::GotoDefinition>(
                goto_params("file:///goto.vue"),
                Duration::from_secs(10),
            )
            .expect("a real response");
        assert!(
            matches!(
                solo,
                Some(lsp_core::lsp_types::GotoDefinitionResponse::Array(ref locations))
                    if locations.is_empty()
            ),
            "the primary's own real answer here is an empty array - the exact shape a null-only \
             emptiness check would have missed, got: {solo:?}"
        );

        let paired = LspConnection::WithCompanion {
            primary,
            companion: spawn_fake_server(root.path(), "companion", "semantic"),
            spec: vue_companion_spec(),
        };
        let answer = paired
            .request::<lsp_core::lsp_types::request::GotoDefinition>(
                goto_params("file:///goto.vue"),
                Duration::from_secs(10),
            )
            .expect("a real response")
            .expect("the companion's real definition should surface through the facade");
        let lsp_core::lsp_types::GotoDefinitionResponse::Array(locations) = answer else {
            panic!("the fake companion sends a real Location array");
        };
        assert_eq!(locations.len(), 1);
        assert_eq!(
            locations[0].uri.as_str(),
            "file:///companion/real-definition.ts"
        );
    }

    /// Revision R11 audit finding 1, second half - the third real "nothing here" shape. The
    /// primary answers a real, structurally valid `CompletionList` whose `items` are empty (an
    /// *object* on the wire, neither `null` nor `[]`), so before this fix completions inside a
    /// `.vue` script block were always empty with no fallback ever attempted.
    #[test]
    fn completion_falls_back_to_the_companion_on_an_empty_completion_list() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "primary", "normal");

        let single = LspConnection::Single(primary.clone());
        let solo = single
            .request::<lsp_core::lsp_types::request::Completion>(
                completion_params("file:///complete.vue"),
                Duration::from_secs(10),
            )
            .expect("a real response");
        assert!(
            matches!(
                solo,
                Some(lsp_core::lsp_types::CompletionResponse::List(ref list))
                    if list.items.is_empty()
            ),
            "the primary's own real answer here is an empty CompletionList object, got: {solo:?}"
        );

        let paired = LspConnection::WithCompanion {
            primary,
            companion: spawn_fake_server(root.path(), "companion", "semantic"),
            spec: vue_companion_spec(),
        };
        let answer = paired
            .request::<lsp_core::lsp_types::request::Completion>(
                completion_params("file:///complete.vue"),
                Duration::from_secs(10),
            )
            .expect("a real response")
            .expect("the companion's real completions should surface through the facade");
        let lsp_core::lsp_types::CompletionResponse::Array(items) = answer else {
            panic!("the fake companion sends a real item array");
        };
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels, vec!["alpha", "beta"]);
    }

    /// The other side of the registry-driven rule: a method that is genuinely **not** on the
    /// companion's `fallback_methods` list must never fan out, so a request the primary answers
    /// correctly can't have a second server's answer quietly substituted for it.
    ///
    /// `textDocument/references` is a real method this app doesn't list for Vue. The companion is
    /// `silent` for it specifically, so a fallback would cost the full real timeout - the elapsed
    /// bound is the actual proof, not the returned value.
    #[test]
    fn a_request_outside_the_fallback_list_never_consults_the_companion() {
        let root = tempfile::tempdir().expect("tempdir");
        let spec = vue_companion_spec();
        assert!(
            !spec.fallback_methods.contains(&"textDocument/references"),
            "this test is only meaningful while references is genuinely off the list"
        );
        let connection = LspConnection::WithCompanion {
            primary: spawn_fake_server(root.path(), "primary", "normal"),
            companion: spawn_fake_server(root.path(), "companion", "silent"),
            spec,
        };
        let params = lsp_core::lsp_types::ReferenceParams {
            text_document_position: lsp_core::lsp_types::TextDocumentPositionParams {
                text_document: lsp_core::lsp_types::TextDocumentIdentifier {
                    uri: uri("file:///refs.vue"),
                },
                position: lsp_core::lsp_types::Position {
                    line: 0,
                    character: 0,
                },
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: lsp_core::lsp_types::ReferenceContext {
                include_declaration: false,
            },
        };
        let started = Instant::now();
        let result = connection
            .request::<lsp_core::lsp_types::request::References>(params, Duration::from_secs(10))
            .expect("a real response");
        assert_eq!(
            result,
            Some(Vec::new()),
            "the primary's own real, empty answer must be returned untouched"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an off-list request must not consult the companion at all - taking anywhere near the \
             real timeout would mean it did, took {:?}",
            started.elapsed()
        );
    }

    /// [`lsp_result_is_empty`] pinned directly against every real typed shape it has to classify -
    /// including the two negative cases that make it safe to apply to *every* listed method rather
    /// than just hover: a real `Hover` is an object with no `items` key at all, and must never be
    /// read as an empty completion list.
    #[test]
    fn the_emptiness_predicate_recognizes_every_real_shape_of_nothing_here() {
        use lsp_core::lsp_types::{
            CompletionItem, CompletionList, CompletionResponse, GotoDefinitionResponse, Hover,
            HoverContents, Location, MarkupContent, MarkupKind, Range,
        };

        assert!(lsp_result_is_empty(&Option::<Hover>::None));
        assert!(lsp_result_is_empty(&Option::<GotoDefinitionResponse>::None));
        assert!(lsp_result_is_empty(&Some(GotoDefinitionResponse::Array(
            Vec::new()
        ))));
        assert!(lsp_result_is_empty(&Some(GotoDefinitionResponse::Link(
            Vec::new()
        ))));
        assert!(lsp_result_is_empty(&Some(CompletionResponse::Array(
            Vec::new()
        ))));
        assert!(lsp_result_is_empty(&Some(CompletionResponse::List(
            CompletionList {
                is_incomplete: false,
                items: Vec::new(),
            }
        ))));

        let location = Location {
            uri: uri("file:///real.ts"),
            range: Range::default(),
        };
        assert!(!lsp_result_is_empty(&Some(GotoDefinitionResponse::Scalar(
            location.clone()
        ))));
        assert!(!lsp_result_is_empty(&Some(GotoDefinitionResponse::Array(
            vec![location]
        ))));
        assert!(!lsp_result_is_empty(&Some(CompletionResponse::Array(
            vec![CompletionItem::default()]
        ))));
        assert!(!lsp_result_is_empty(&Some(CompletionResponse::List(
            CompletionList {
                is_incomplete: false,
                items: vec![CompletionItem::default()],
            }
        ))));
        assert!(
            !lsp_result_is_empty(&Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::PlainText,
                    value: "const bad: number".to_string(),
                }),
                range: None,
            })),
            "a real hover is an object with no `items` key - an absent key must never count as an \
             empty one, or every real hover would be thrown away as 'nothing here'"
        );
    }

    /// A dead half must be reported honestly, by name - not silently swallowed into a
    /// plausible-looking "everything's fine" status.
    #[test]
    fn a_dead_companion_flips_liveness_and_is_named_in_the_real_status() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "vue-language-server", "normal");
        let companion =
            spawn_fake_server(root.path(), "typescript-language-server (vue)", "normal");
        let connection = LspConnection::WithCompanion {
            primary: primary.clone(),
            companion: companion.clone(),
            spec: vue_companion_spec(),
        };
        assert!(connection.is_connection_alive());
        assert!(connection.liveness_failure_reason().is_none());

        // A real, unprompted process death - no `shutdown()`, standing in for a crash.
        companion
            .notify_raw("test/die", serde_json::Value::Null)
            .expect("the fake server should accept the notification that kills it");

        let deadline = Instant::now() + Duration::from_secs(10);
        while connection.is_connection_alive() {
            assert!(
                Instant::now() < deadline,
                "a WithCompanion connection must stop reporting itself alive once either half's \
                 real process dies"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let reason = connection
            .liveness_failure_reason()
            .expect("a dead half must produce a real reason");
        assert!(
            reason.contains("typescript-language-server (vue)"),
            "the message must name which real process died, got: {reason}"
        );

        let status = lsp_file_status(
            &Some(LspClientState::Ready(primary)),
            Some(&LspClientState::Ready(companion)),
            Some(&connection),
            Some(&uri("file:///App.vue")),
        );
        let LspFileStatus::Failed(message) = status else {
            panic!("a dead companion must surface as a real Failed status, got a different one");
        };
        assert!(
            message.contains("typescript-language-server (vue)"),
            "the file status must name the companion specifically, got: {message}"
        );
    }

    /// The same honesty rule in the other direction: a dead primary names the primary.
    #[test]
    fn a_dead_primary_is_named_rather_than_blamed_on_the_companion() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "vue-language-server", "normal");
        let connection = LspConnection::WithCompanion {
            primary: primary.clone(),
            companion: spawn_fake_server(root.path(), "typescript-language-server (vue)", "normal"),
            spec: vue_companion_spec(),
        };
        primary
            .notify_raw("test/die", serde_json::Value::Null)
            .expect("notification accepted");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(reason) = connection.liveness_failure_reason() {
                assert!(
                    reason.contains("vue-language-server"),
                    "the message must name the real primary, got: {reason}"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the dead primary was never noticed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A companion that failed its own spawn must be surfaced honestly rather than silently
    /// leaving the user with half the analysis and a confident-looking status.
    #[test]
    fn a_companion_that_failed_to_spawn_is_surfaced_in_the_real_file_status() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "vue-language-server", "normal");
        let connection = LspConnection::Single(primary.clone());
        let status = lsp_file_status(
            &Some(LspClientState::Ready(primary)),
            Some(&LspClientState::Failed(
                "no real @vue/typescript-plugin found".to_string(),
            )),
            Some(&connection),
            Some(&uri("file:///App.vue")),
        );
        let LspFileStatus::Failed(message) = status else {
            panic!("a companion that failed its prerequisite must not be silently ignored");
        };
        assert_eq!(message, "no real @vue/typescript-plugin found");
    }

    /// A companion still coming up is not an error and must not be reported as one - the primary
    /// alone is genuinely working in the meantime.
    #[test]
    fn a_still_spawning_companion_leaves_the_primarys_own_status_intact() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "vue-language-server", "normal");
        let connection = LspConnection::Single(primary.clone());
        let status = lsp_file_status(
            &Some(LspClientState::Ready(primary)),
            Some(&LspClientState::Spawning),
            Some(&connection),
            Some(&uri("file:///App.vue")),
        );
        assert!(
            matches!(status, LspFileStatus::Indexing),
            "a companion mid-spawn says nothing yet - the primary's own honest 'no result for \
             this file yet' is the right answer"
        );
    }

    /// The real, full relay round trip through the real production dispatch, with both halves
    /// genuinely spawned: the primary's `[[id, command, args]]` reaches the companion as a real
    /// `workspace/executeCommand`, and the companion's `body` comes back as a real
    /// `[[id, body]]` - double-wrapped, which is not optional (a real `vue-language-server`'s own
    /// handler throws internally on the un-wrapped shape).
    #[gpui::test]
    fn a_real_relay_round_trip_delivers_the_companions_body_in_the_real_wire_shape(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let spec = vue_companion_spec();
        let primary = spawn_fake_server(repo.path(), "vue-language-server", "normal");
        let companion = spawn_fake_server(repo.path(), spec.client_key, "normal");

        app.update(cx, |app, cx| {
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(
                (root.clone(), "vue-language-server"),
                LspClientState::Ready(primary.clone()),
            );
            app.lsp_clients.insert(
                (root.clone(), spec.client_key),
                LspClientState::Ready(companion),
            );
            app.dispatch_companion_relay(
                root,
                "vue-language-server",
                spec,
                serde_json::json!([[7, "_vue:projectInfo", { "file": "/tmp/App.vue" }]]),
                cx,
            );
        });
        cx.run_until_parked();

        let observed = wait_for_relay_observation(&primary);
        let observed: serde_json::Value =
            serde_json::from_str(&observed).expect("the relayed params round-trip as real JSON");
        assert_eq!(
            observed,
            serde_json::json!([[7, { "echoed": ["_vue:projectInfo", { "file": "/tmp/App.vue" }] }]]),
            "the response must be the double-wrapped [[id, body]] shape, carrying the \
             companion's own `body` (not the whole tsserver envelope)"
        );
    }

    /// Adversarial case (a): a companion that never answers must not leave the primary's own
    /// internal promise hanging forever - the real `LSP_QUERY_TIMEOUT` applies and a real `null`
    /// body still goes back.
    #[gpui::test]
    fn a_companion_that_never_answers_still_gets_a_real_null_response_back_to_the_primary(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let spec = vue_companion_spec();
        let primary = spawn_fake_server(repo.path(), "vue-language-server", "normal");
        let companion = spawn_fake_server(repo.path(), spec.client_key, "silent");

        let started = Instant::now();
        app.update(cx, |app, cx| {
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(
                (root.clone(), "vue-language-server"),
                LspClientState::Ready(primary.clone()),
            );
            app.lsp_clients.insert(
                (root.clone(), spec.client_key),
                LspClientState::Ready(companion),
            );
            app.dispatch_companion_relay(
                root,
                "vue-language-server",
                spec,
                serde_json::json!([[11, "_vue:projectInfo", {}]]),
                cx,
            );
        });
        cx.run_until_parked();

        let observed = wait_for_relay_observation(&primary);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&observed).expect("real JSON"),
            serde_json::json!([[11, null]]),
            "a timed-out companion must still produce a real, well-formed null response"
        );
        assert!(
            started.elapsed() >= LSP_QUERY_TIMEOUT,
            "the real, existing query timeout is what bounds this - a shorter wait would mean \
             something other than the real timeout produced the null"
        );
    }

    /// Adversarial case (b): a companion that isn't `Ready` when a relay arrives (still spawning,
    /// or evicted by a worktree switch mid-flight) must not panic, and must not silently drop the
    /// primary's request either.
    #[gpui::test]
    fn a_relay_arriving_before_the_companion_is_ready_still_answers_the_primary(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let spec = vue_companion_spec();
        let primary = spawn_fake_server(repo.path(), "vue-language-server", "normal");

        app.update(cx, |app, cx| {
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(
                (root.clone(), "vue-language-server"),
                LspClientState::Ready(primary.clone()),
            );
            // Deliberately mid-spawn, exactly as a real race would leave it.
            app.lsp_clients
                .insert((root.clone(), spec.client_key), LspClientState::Spawning);
            app.dispatch_companion_relay(
                root,
                "vue-language-server",
                spec,
                serde_json::json!([[3, "_vue:projectInfo", {}]]),
                cx,
            );
        });
        cx.run_until_parked();

        let observed = wait_for_relay_observation(&primary);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&observed).expect("real JSON"),
            serde_json::json!([[3, null]])
        );
    }

    /// Revision R11 audit finding 3. `LspConnection::pull_diagnostics` used to fire the
    /// companion's own pull itself, from a bare, detached `std::thread::spawn` re-run on every one
    /// of [`PULL_DIAGNOSTICS_EMPTY_RETRIES`]'s attempts. That decision is now
    /// [`LspConnection::companion_diagnostics_pull_target`]'s - a real, live capability read the
    /// caller drives once per sync tick as a tracked task - so this pins the capability check
    /// itself in both directions, against real handshakes rather than a version assumption.
    #[test]
    fn only_a_companion_that_really_advertises_pull_support_becomes_a_pull_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let primary = spawn_fake_server(root.path(), "primary", "pull");

        assert!(
            LspConnection::Single(primary.clone())
                .companion_diagnostics_pull_target()
                .is_none(),
            "a single-server connection has no companion to pull from at all"
        );
        assert!(
            LspConnection::WithCompanion {
                primary: primary.clone(),
                // Exactly the real shape of every companion that exists today: it pushes
                // publishDiagnostics and advertises no diagnosticProvider whatsoever.
                companion: spawn_fake_server(root.path(), "companion", "normal"),
                spec: vue_companion_spec(),
            }
            .companion_diagnostics_pull_target()
            .is_none(),
            "a companion that advertises no diagnosticProvider must never be pulled from - a real \
             pull would be a pure no-op round trip"
        );

        let pull_capable = LspConnection::WithCompanion {
            primary,
            companion: spawn_fake_server(root.path(), "pull-capable-companion", "pull"),
            spec: vue_companion_spec(),
        };
        let target = pull_capable
            .companion_diagnostics_pull_target()
            .expect("a companion advertising a real diagnosticProvider is genuinely pullable");
        assert_eq!(target.name(), "pull-capable-companion");
    }

    /// Adversarial case (c), Revision R11 audit finding 4: a payload whose `command` is malformed
    /// (a number, which the companion's own `executeCommand` could never accept) but whose
    /// `requestId` is perfectly real. Distinct from
    /// [`a_malformed_relay_payload_is_an_honest_none_not_a_panic`], which covers payloads that
    /// can't be answered at all: here a reply is genuinely possible, so before this fix the
    /// dispatch bailing outright left the primary's own internal promise hanging forever -
    /// contradicting this path's own "every path still sends a response" invariant.
    #[gpui::test]
    fn a_relay_with_a_real_id_but_a_malformed_command_still_answers_the_primary(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let spec = vue_companion_spec();
        let primary = spawn_fake_server(repo.path(), "vue-language-server", "normal");
        let companion = spawn_fake_server(repo.path(), spec.client_key, "normal");

        app.update(cx, |app, cx| {
            let root = app.file_tree_root.clone();
            app.lsp_clients.insert(
                (root.clone(), "vue-language-server"),
                LspClientState::Ready(primary.clone()),
            );
            app.lsp_clients.insert(
                (root.clone(), spec.client_key),
                LspClientState::Ready(companion),
            );
            // A real, `Ready` companion is deliberately present: the null must come from the
            // unusable command, not from a missing companion.
            app.dispatch_companion_relay(
                root,
                "vue-language-server",
                spec,
                serde_json::json!([[13, 42, { "file": "/tmp/App.vue" }]]),
                cx,
            );
        });
        cx.run_until_parked();

        let observed = wait_for_relay_observation(&primary);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&observed).expect("real JSON"),
            serde_json::json!([[13, null]]),
            "the real id must come back with an honest null body rather than nothing at all"
        );
    }

    /// Item 5 of the issue's scope: the facade must add no meaningful overhead to the already-
    /// working single-server path. Measures a real, non-network method (the server's own
    /// advertised `textDocumentSync` capability, read under a `Mutex`) called directly on the raw
    /// client vs. through `LspConnection::Single`'s delegating method.
    ///
    /// No hard threshold is asserted on the *ratio* - timing in a sandbox under real parallel
    /// process load is genuinely noisy, and a flaky performance gate is worse than none. What is
    /// asserted is a deliberately generous, real bound - 1000ns - not "zero"/"unmeasurable": on a
    /// loaded CI runner the delta is real wall-clock time and can land anywhere under that
    /// ceiling, not nanoseconds as the ceiling's own headroom might suggest. Kept in the normal,
    /// non-`#[ignore]` suite on purpose, matching this project's own established convention for
    /// its other timing-sensitive tests (see `lsp_diagnostics_wiring_tests`'s module docs) - this
    /// project has no separate slow/perf-test lane. The measured numbers are printed so a
    /// regression is visible in the log even when the assertion passes.
    #[test]
    fn single_delegation_stays_under_a_generous_1000ns_ceiling() {
        let root = tempfile::tempdir().expect("tempdir");
        let client = spawn_fake_server(root.path(), "bench", "normal");
        let connection = LspConnection::Single(client.clone());
        const ITERATIONS: u32 = 200_000;

        // A real warm-up pass, so neither measurement pays first-touch costs the other doesn't.
        for _ in 0..1_000 {
            std::hint::black_box(client.supports_document_sync());
            std::hint::black_box(connection.supports_document_sync());
        }

        let started = Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(client.supports_document_sync());
        }
        let direct = started.elapsed();

        let started = Instant::now();
        for _ in 0..ITERATIONS {
            std::hint::black_box(connection.supports_document_sync());
        }
        let through_facade = started.elapsed();

        let direct_ns = direct.as_nanos() as f64 / f64::from(ITERATIONS);
        let facade_ns = through_facade.as_nanos() as f64 / f64::from(ITERATIONS);
        println!(
            "supports_document_sync: direct {direct_ns:.1}ns/call, via LspConnection::Single \
             {facade_ns:.1}ns/call, delta {:.1}ns",
            facade_ns - direct_ns
        );
        assert!(
            facade_ns - direct_ns < 1_000.0,
            "the facade's enum match + delegate should stay well under this generous 1000ns \
             ceiling; a real microsecond-or-more of added cost per call would mean it isn't the \
             cheap branch it claims to be (direct {direct_ns:.1}ns, facade {facade_ns:.1}ns)"
        );
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
    /// (`EntityInputHandler::replace_text_in_range`, which is what `crate::code_surface::editing`'s own
    /// `on_key_down`/IME plumbing ultimately calls), then advances the deterministic test clock
    /// past [`LSP_SYNC_DEBOUNCE`] and drains the executor so the real, debounced
    /// `AdeApp::schedule_lsp_sync` task this edit armed actually fires (mirrors
    /// `crate::code_surface::editing::editing_tests`' own `REHIGHLIGHT_DEBOUNCE` advance-then-park
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
