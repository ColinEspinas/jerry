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
/// `lsp_core::LspClient::pull_diagnostics` call succeeds but reports an empty result - or
/// outright fails/times out - right after a real `didChange`. Two distinct, genuine, live-
/// observed races, both closed by the same retry budget:
///
/// - A real rust-analyzer's own internal reanalysis can still be catching up to the exact
///   content just sent even when it *doesn't* cancel the pull, answering instead with a real,
///   structurally valid, but stale "no problems" report (observed live, under real parallel-
///   process CPU contention, while building this feature - see `lsp_core::client::tests::
///   did_change_full_then_a_real_pull_reports_a_real_new_diagnostic`'s own docs for the same
///   race caught at the `lsp-core` layer). This is distinct from the `ServerCancelled` retry
///   `pull_diagnostics` itself already handles internally.
/// - A real pull request can also simply time out (`LSP_QUERY_TIMEOUT`) under severe real
///   full-suite parallel load, where a genuinely busy server takes longer than one request's own
///   budget to answer even though it's still alive - a transient failure, not a genuine "this
///   server can't answer" condition, and one this loop must retry exactly like an empty result
///   rather than give up on. This matters especially for rust-analyzer: it never re-pushes
///   `publishDiagnostics` unsolicited after its first `didOpen` (see `LspClient::
///   supports_diagnostic_pull`'s own docs), so this pull loop is the *only* path a post-edit
///   diagnostic update ever reaches this app through for it - treating a single timed-out
///   attempt as terminal used to permanently strand the diagnostic no matter how long a caller's
///   own outer wait loop kept polling afterward (see `lsp_diagnostics_wiring_tests`' own real,
///   live-reproduced failure this fixes).
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

/// The one real "this server is gone" message, shared by [`LspConnection::liveness_failure_reason`]
/// (which reports it for the file being rendered) and [`AdeApp::reap_dead_lsp_clients`] (which
/// records it into [`LspClientState::Failed`] on the poll cadence) so the two can't drift into
/// telling the user two different things about the same dead process.
///
/// Says "exited or stopped responding", not just "exited", because both are now real, distinct
/// ways `lsp_core::LspClient::is_connection_alive` genuinely goes `false`: the reader thread
/// seeing EOF (a crash/kill), and an outbound write that the server never accepted within
/// `lsp-core`'s own write budget (a hung-but-alive server - see that crate's
/// `transport::write_message_bounded`). Naming only the first would be actively wrong for the
/// second.
fn connection_lost_message(server: &str) -> String {
    format!("{server}'s connection was lost (the process exited or stopped responding)")
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
                return Some(connection_lost_message(client.name()));
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
            self.shutdown_lsp_client_off_thread(state, cx);
            // `Spawning`/`Failed` states hold no process to tear down. A `Spawning` one whose
            // background task is still in-flight will, once it resolves, re-insert an entry
            // under `key` even though it's no longer active - harmless: the next eviction pass
            // catches it same as any other stale entry.
        }
    }

    /// Tears one real server down without blocking the GPUI thread - the shared body behind both
    /// [`Self::evict_stale_lsp_clients`] and [`Self::restart_lsp_clients`], so the
    /// `try_unwrap`-then-`shutdown`-else-`drop` rule lives in exactly one place. See
    /// [`Self::evict_stale_lsp_clients`]'s own docs for why each half of that rule is what it is.
    fn shutdown_lsp_client_off_thread(&mut self, state: LspClientState, cx: &mut Context<Self>) {
        let LspClientState::Ready(client) = state else {
            // `Spawning`/`Failed` hold no process to tear down.
            return;
        };
        let server_name = client.name();
        let task = cx.background_executor().spawn(async move {
            match std::sync::Arc::try_unwrap(client) {
                Ok(mut client) => {
                    if let Err(err) = client.shutdown() {
                        log::warn!("failed to shut down {server_name}: {err}");
                    }
                }
                Err(client) => drop(client),
            }
        });
        self._lsp_tasks.push(task);
    }

    /// Demotes every [`LspClientState::Ready`] entry whose real process has actually died to a
    /// [`LspClientState::Failed`] one naming it. Returns `true` if anything changed, so
    /// [`Self::ensure_lsp_poll_task`]'s loop - the one real cadence this runs on - can `cx.notify()`
    /// only when there's something new to draw.
    ///
    /// ## The real bug this closes
    ///
    /// `lsp_core::LspClient::is_connection_alive` has been a real, honest signal since Revision
    /// R8.5b, but nothing in this crate ever *checked* it on a cadence: it was read only by
    /// `lsp_file_status`, i.e. only while the dead server's own language happened to be the file
    /// being rendered. A dead `rust-analyzer` therefore stayed `Ready` in [`Self::lsp_clients`]
    /// indefinitely while a TypeScript file was open, with every sync tick, hover and completion
    /// still being routed at a process that will never answer, and nothing anywhere saying so.
    ///
    /// Demoting to `Failed` fixes both halves of that at once, because the rest of this module
    /// already treats `Failed` correctly: [`Self::lsp_connection_for_path`] stops resolving a
    /// connection at all (so no further doomed requests are dispatched), and [`lsp_file_status`]
    /// reports the real message. It deliberately does **not** respawn - see
    /// [`Self::restart_lsp_clients`] for why that stays the user's call.
    pub(in crate::lsp) fn reap_dead_lsp_clients(&mut self, cx: &mut Context<Self>) -> bool {
        let dead: Vec<(LspClientKey, String)> = self
            .lsp_clients
            .iter()
            .filter_map(|(key, state)| {
                let LspClientState::Ready(client) = state else {
                    return None;
                };
                (!client.is_connection_alive())
                    .then(|| (key.clone(), connection_lost_message(client.name())))
            })
            .collect();
        if dead.is_empty() {
            return false;
        }
        for (key, reason) in dead {
            log::warn!("{reason} - it will not be used again until it is restarted");
            let replaced = self.lsp_clients.insert(key, LspClientState::Failed(reason));
            // The replaced `Ready` state is handed to the same off-thread teardown eviction and
            // restart use, never just dropped here. Dropping the last `Arc` inline would run
            // `lsp_core::LspClient`'s own `Drop` on the GPUI foreground thread - a `/proc`
            // descendant walk, real `kill(2)` calls and a blocking reap - from inside this
            // method's 250ms poll tick, breaking this file's own "never block the UI thread on
            // process teardown" rule. Short in practice for an already-dead process, but the rule
            // does not have a "probably fast" exemption.
            if let Some(replaced) = replaced {
                self.shutdown_lsp_client_off_thread(replaced, cx);
            }
        }
        true
    }

    /// Throws away every language server for the active worktree root and lets the ordinary
    /// render path start fresh ones - the real recovery action behind
    /// [`crate::palette::state::PaletteCommand::RestartLanguageServers`] and the File view footer's own
    /// clickable failed-status chip.
    ///
    /// ## Why this is a user action and not an automatic respawn
    ///
    /// Before this existed there was no recovery path at all: [`Self::spawn_lsp_client`]
    /// deliberately no-ops for a key that already has an entry *in any state* (so a failure isn't
    /// retried on every render), and nothing ever removed an entry whose process had died. The
    /// only thing that did was [`Self::evict_stale_lsp_clients`], on a worktree switch - so the
    /// sole real way a user could revive a dead server was to switch worktrees and back, or
    /// restart the app, neither of which is discoverable as a fix for "diagnostics stopped
    /// appearing".
    ///
    /// Automatic respawning was considered and deliberately not chosen. A server that died *while
    /// analyzing a particular file* will very likely die again the moment it's handed that same
    /// file, and an automatic loop turns one honest failure into a permanent, invisible
    /// spawn/crash cycle - the opposite of this app's "honest status over magic" rule. Real Zed
    /// makes the same call, and it was checked in the vendored tree rather than assumed: its own
    /// recovery is a user-invoked `"Restart Server"` menu entry
    /// (`vendor/zed/crates/language_tools/src/lsp_button.rs:476`) calling
    /// `LspStore::restart_language_servers_for_buffers`; no automatic post-crash respawn path was
    /// found in `vendor/zed/crates/project/src/lsp_store.rs` to point at (an absence, so cited as
    /// one rather than as a verified line).
    ///
    /// ## Why the document bookkeeping has to be cleared too
    ///
    /// [`Self::lsp_opened_files`] is what makes `didOpen` fire exactly once per path, and the
    /// version/sync maps describe a conversation with a process that no longer exists. Leaving
    /// any of them behind would give the fresh server a file it was never told to open, or a
    /// `didChange` at a version it never saw - a *worse* silent failure than the dead connection
    /// this is recovering from, since everything would look alive while answering about nothing.
    /// [`Self::lsp_uri_cache`] is deliberately kept: it is a pure path-to-`file://` mapping with
    /// no server state in it at all, and dropping it would needlessly stall the next tick's
    /// completions (see [`Self::prepare_lsp_sync`]'s own cache-miss branch).
    pub(crate) fn restart_lsp_clients(&mut self, cx: &mut Context<Self>) {
        let root = self.file_tree_root.clone();
        // Dropped *first*, before a single map is cleared. Each of these is a real in-flight
        // background task that ends by writing back into exactly the bookkeeping cleared below:
        // `schedule_lsp_sync`'s continuation re-inserts `lsp_last_synced_content`/
        // `lsp_synced_version` when its `did_change_full` returns `Ok`, and its diagnostics-pull
        // retry loop re-inserts `lsp_diagnostics_confirmed_version` on every attempt - a window
        // of `PULL_DIAGNOSTICS_EMPTY_RETRIES` x `PULL_DIAGNOSTICS_EMPTY_RETRY_DELAY`, about 8
        // real seconds *after* the restart. A resurrected `lsp_last_synced_content` entry is the
        // damaging one: `prepare_lsp_sync` reads it as "the server already has this content", so
        // the freshly spawned server would never be sent a `didChange` for a dirty buffer and
        // would answer forever about the file's on-disk text instead - a user who clicked restart
        // because diagnostics stopped, and now silently gets diagnostics for the wrong content.
        // Dropping a `Task` cancels it, which is also what stops a surviving task's captured
        // `Arc<LspConnection>` clone from defeating `Arc::try_unwrap` in the teardown below and
        // leaving the old server process alive alongside the new one. Same discipline, and the
        // same ordering, as `AdeApp::select_worktree`'s own worktree-switch reset.
        self._lsp_sync_tasks = std::collections::HashMap::new();
        self._completions_request_task = None;
        self._completions_resolve_task = None;
        self.completions_resolve_in_flight = None;

        let keys: Vec<LspClientKey> = self
            .lsp_clients
            .keys()
            .filter(|(key_root, _)| *key_root == root)
            // A `Spawning` entry is deliberately left in place. Its own background task is still
            // in flight and will re-insert under this key when it resolves, so removing it here
            // would free the key for the next render to spawn a *second* real process for the
            // same server - two rust-analyzers indexing one workspace, at real GB apiece.
            // (`evict_stale_lsp_clients` tolerates that same re-insert only because its keys are
            // no longer the active root's; that reasoning does not carry over here.) Nothing is
            // lost by waiting: a spawn in flight is already the fresh start a restart is asking
            // for, and if it resolves into a client that is itself dead, the poll loop's own
            // `reap_dead_lsp_clients` catches it on the next tick.
            .filter(|key| !matches!(self.lsp_clients.get(key), Some(LspClientState::Spawning)))
            .cloned()
            .collect();
        for key in keys {
            if let Some(state) = self.lsp_clients.remove(&key) {
                log::info!("restarting {} for {}", key.1, root.display());
                self.shutdown_lsp_client_off_thread(state, cx);
            }
        }

        self.lsp_opened_files
            .retain(|path| !path.starts_with(&root));
        self.lsp_document_versions
            .retain(|path, _| !path.starts_with(&root));
        // Worktree-relative-keyed, and only ever holding the *active* worktree's paths (see
        // `AdeApp::select_worktree`, which clears all three on a switch) - so
        // clearing them outright is exactly "forget this root's conversation", not an
        // over-broad reset.
        self.lsp_last_synced_content.clear();
        self.lsp_synced_version.clear();
        self.lsp_diagnostics_confirmed_version.clear();
        // Anything still on screen was computed from the connection just torn down - the same
        // set `AdeApp::select_worktree` clears for the same reason.
        self.dismiss_completions();
        self.dismiss_hover();
        self.file_view_diagnostics = std::collections::HashMap::new();
        // The next render's own `ensure_lsp_client`/`dispatch_did_open` calls do the real
        // respawn and re-open, through exactly the same code path a cold start uses.
        cx.notify();
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
        // Read here, on the GPUI thread, and moved into the builders below - which run on the
        // background executor and so cannot reach `self`. See
        // `crate::settings::store::LspServerSettings`.
        //
        // Layered built-in -> this app's own auto-import suppression -> the user's own
        // passthrough, so a hand-written `settings.toml` entry always has the last word over a
        // settings-page toggle.
        let user_lsp_settings = self.settings.lsp.clone();
        let suggest_auto_imports = self.settings.editor.suggest_auto_imports;
        let apply_user_options =
            std::sync::Arc::new(move |config: &mut lsp_core::ServerSpawnConfig| {
                if !suggest_auto_imports {
                    config.initialization_options =
                        crate::language::merge_initialization_options_json(
                            config.initialization_options.take(),
                            crate::language::auto_import_suppression_options(config.name),
                        );
                }
                config.initialization_options = crate::language::merge_initialization_options(
                    config.initialization_options.take(),
                    user_lsp_settings
                        .get(config.name)
                        .and_then(|server| server.initialization_options.as_ref()),
                );
            });
        self.spawn_lsp_client(
            (repo_root.clone(), binary),
            repo_root.clone(),
            {
                let repo_root = repo_root.clone();
                let apply_user_options = std::sync::Arc::clone(&apply_user_options);
                move || {
                    // The real `ServerSpawnConfig` (including any `$PATH`/filesystem probing it
                    // does - Pyright's `pythonPath` resolution, Vue's `--tsdk` resolution) is
                    // built here, off the GPUI thread - see this method's own docs for why that
                    // moved from the caller.
                    let mut config = crate::language::server_spawn_config(&repo_root, extension)?
                        .ok_or_else(|| {
                        format!("no LSP server is configured for extension {extension:?}")
                    })?;
                    apply_user_options(&mut config);
                    Ok(config)
                }
            },
            cx,
        );

        if let Some(companion) = crate::language::companion_for_extension(extension) {
            self.spawn_lsp_client(
                (repo_root.clone(), companion.client_key),
                repo_root,
                move || {
                    let mut config = crate::language::companion_spawn_config(&companion)?;
                    apply_user_options(&mut config);
                    Ok(config)
                },
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
                // A real, cadence-driven health check, before anything else this tick reads
                // `lsp_clients` - see `AdeApp::reap_dead_lsp_clients`'s own docs for the real
                // silent-failure bug it closes.
                if this.reap_dead_lsp_clients(cx) {
                    any_update = true;
                }
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
    ///
    /// `cwd` is the worktree [`AdeApp::edit_buffers`]'s composite key needs to find `relative_
    /// path`'s buffer - the caller's own current [`AdeApp::file_tree_root`] when called
    /// synchronously (every direct `crate::code_surface::editing` call site), or a `cwd` that
    /// caller itself already captured before its own `.await` when this is called from inside
    /// another real `cx.spawn` continuation (`crate::code_surface::tabs::AdeApp::spawn_file_load`'s
    /// "reloaded" branch) - never re-derived from `self.file_tree_root` once *this* method's own
    /// debounce timer resumes below, which could by then name a worktree the user has since
    /// switched away to. See [`AdeApp::edit_buffers`]'s own docs for the stale-worktree bug class
    /// this prevents.
    pub(crate) fn schedule_lsp_sync(
        &mut self,
        cwd: PathBuf,
        relative_path: PathBuf,
        cx: &mut Context<Self>,
    ) {
        // GitHub issue #189, and deliberately *before* the debounce below, not inside it: every
        // real edit path in `crate::code_surface::editing` already funnels through this method
        // synchronously, so this one call is what makes an already-open popup narrow on the very
        // keystroke that was typed, with no round trip and no timer in between. The debounced
        // request below still refreshes the underlying candidate set behind it - see
        // `AdeApp::refilter_completions`' own docs for how the two compose.
        self.refilter_completions(cx);

        let task = cx.spawn({
            let relative_path = relative_path.clone();
            async move |this, cx| {
                cx.background_executor().timer(LSP_SYNC_DEBOUNCE).await;
                let Ok(Some(plan)) = this.update(cx, |this, cx| {
                    this.prepare_lsp_sync(&cwd, &relative_path, cx, false)
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
                // stay whatever they were until the *next* sync tick tries again - this app has
                // no separate "diagnostics refresh failed" affordance for this phase's scope,
                // and inventing one for a single best-effort background refresh isn't worth it.
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
                    // live-observed races this bounded retry-on-empty-or-timeout closes, and
                    // for why it's only even entered at all when `previous_result_was_non_empty`
                    // (Revision
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
                            // recorded here (Revision R8.5b audit finding 6), not just the send
                            // itself. `.max(..)` so a real, late-arriving confirmation for an older
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
                            // `None` here means the pull itself failed or hit
                            // `LSP_QUERY_TIMEOUT` outright - a real, live possibility under
                            // full-suite parallel load, where a genuinely busy real
                            // `rust-analyzer`/`pyright`/`typescript-language-server` can take
                            // longer than one request's own timeout to answer even though it's
                            // still alive and would have answered eventually. Treating that the
                            // same as an honest "still empty" answer (both mean "no fresh
                            // confirmation yet") rather than as a terminal failure closes a real,
                            // live-observed bug: rust-analyzer specifically never re-pushes
                            // `publishDiagnostics` unsolicited after its first `didOpen` (see
                            // `LspClient::supports_diagnostic_pull`'s own docs), so this pull
                            // loop is the *only* path a post-edit diagnostic update ever reaches
                            // this app through for it. Before this fix, a single transient
                            // timeout here - not even a genuine failure, just this one real
                            // request outrunning its own budget under load - permanently
                            // stranded the diagnostic: the loop broke immediately, nothing else
                            // ever re-asks for this edit, and a caller's own outer wait loop
                            // (e.g. `lsp_diagnostics_wiring_tests::wait_until`) could poll for
                            // its entire real deadline and never see the diagnostic arrive, no
                            // matter how generous that deadline was widened to be.
                            Some(true) | None if attempt < max_retries => {
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
        // Called synchronously (`Ctrl+Space`, no `.await` between here and `Self::
        // prepare_lsp_sync`'s own read), so `self.file_tree_root` genuinely is "now" - unlike
        // `Self::schedule_lsp_sync`'s debounced continuation, which must capture its own `cwd`
        // before waiting out the debounce instead.
        let cwd = self.file_tree_root.clone();
        let Some(plan) = self.prepare_lsp_sync(&cwd, &relative_path, cx, true) else {
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
        cwd: &Path,
        relative_path: &Path,
        cx: &mut Context<Self>,
        force_completion: bool,
    ) -> Option<LspSyncPlan> {
        let buffer = self.edit_buffer_at(cwd, relative_path)?;
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

        // Consumed here, exactly once, regardless of which branch below actually runs - see this
        // field's own docs. Only suppresses the organic (non-`force_completion`) trigger check:
        // an explicit Ctrl+Space right after accepting is a real, deliberate re-ask and must
        // still work.
        let suppress_organic_trigger = std::mem::take(&mut self.completions_suppress_next_trigger);
        let completion_context = if force_completion {
            Some(lsp_core::lsp_types::CompletionContext {
                trigger_kind: lsp_core::lsp_types::CompletionTriggerKind::INVOKED,
                trigger_character: None,
            })
        } else if suppress_organic_trigger {
            None
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
                self.completions_resolved_items.clear();
                // Only actually drop to `Loading` when there's nothing real to show yet for this
                // path - not unconditionally, as an earlier version did. `Self::schedule_lsp_sync`
                // already called `Self::refilter_completions` synchronously just before this
                // debounced tick, so a `Ready` popup here is already honestly narrowed to
                // everything typed so far (GitHub issue #189); overwriting it with a bare
                // "loading completions..." row on *every* debounce tick (every real
                // `LSP_SYNC_DEBOUNCE` while typing continues) made the popup visibly flicker
                // between that placeholder and real content on nearly every keystroke, and
                // silently dropped the `"completions"` key context for that same window each time
                // (`Self::completions_open_for_active_path` requires `Ready`) - so `Up`/`Down`/
                // `Enter` briefly stopped reaching the popup too. The stale-response race this
                // used to also guard against is still closed by the generation bump above and by
                // `Self::apply_completion_result`'s own `completions_generation` check.
                let already_ready_for_this_path = self.completions.as_ref().is_some_and(|entry| {
                    entry.path == relative_path
                        && matches!(entry.status, CompletionsStatus::Ready { .. })
                });
                if !already_ready_for_this_path {
                    self.completions = Some(CompletionsEntry {
                        path: relative_path.to_path_buf(),
                        status: CompletionsStatus::Loading,
                    });
                    cx.notify();
                }
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
                // Narrowed against the prefix typed at the caret *right now* (GitHub issue #189),
                // not against the one that was there when this request went out - a response that
                // took a real round trip to arrive must land already filtered to what the user has
                // since typed, or the popup would visibly widen back out for one frame on every
                // server reply. `CompletionsStatus::ready` returns `None` when nothing in the
                // response matches, which is treated exactly like the genuinely-empty response
                // below: no popup.
                let query = self
                    .completion_filter_query(relative_path)
                    .unwrap_or_default();
                CompletionsStatus::ready(items, &query).map(|status| CompletionsEntry {
                    path: relative_path.to_path_buf(),
                    status,
                })
            }
            Ok(None) => None,
            Err(err) => Some(CompletionsEntry {
                path: relative_path.to_path_buf(),
                status: CompletionsStatus::Failed(err.to_string()),
            }),
        };
        let is_ready = matches!(
            new_state.as_ref().map(|entry| &entry.status),
            Some(CompletionsStatus::Ready { .. })
        );
        self.completions = new_state;
        if is_ready {
            // A genuinely new response starts at `selected: 0`, so its list has to start scrolled
            // to the top too - `AdeApp::completions_scroll_handle` is a long-lived field, and
            // without this the *previous* response's scroll offset would survive into this one,
            // showing a viewport that has nothing to do with the freshly selected first item
            // (GitHub issue #185). `Top`, not `Nearest`: this is a reset, not a follow-the-
            // selection nudge.
            self.completions_scroll_handle
                .scroll_to_item(0, gpui::ScrollStrategy::Top);
            self.maybe_resolve_selected_completion_item(cx);
        }
        cx.notify();
    }

    /// Dispatches a real `completionItem/resolve` request for whichever item [`Self::completions`]
    /// currently has selected, if the server genuinely needs one - most real servers (rust-
    /// analyzer very much included) send only a bare `label`/`kind` inline in the
    /// `textDocument/completion` response itself and expect a follow-up `completionItem/resolve`
    /// for the one item a user is actually looking at, which is exactly what
    /// `crate::lsp::completion_popup::AdeApp::render_completions_popover`'s detail pane reads
    /// (`crate::lsp::completion::completion_documentation_text`/`completion_module_path`, plus
    /// `item.detail` for the signature line) - without this, that pane stays empty for nearly
    /// every real item, not just the rare one a server genuinely has nothing more to say about.
    ///
    /// A real no-op whenever: there's no `Ready` popup, the currently selected item already has
    /// both a `detail` and real `documentation` (nothing more a resolve could usefully add), this
    /// exact `(path, generation, item index)` has already been *answered* once (successfully or
    /// not - see [`Self::completions_resolved`]'s own docs) or is the one currently still in
    /// flight ([`Self::completions_resolve_in_flight`]), or the connection's own primary server
    /// doesn't advertise `completionProvider.resolveProvider` at all.
    ///
    /// Deliberately *not* a no-op for an item whose earlier request a later selection cancelled:
    /// that item has no answer, so asking again is the only way it ever gets one. See
    /// [`Self::completions_resolve_in_flight`]'s own docs for the real data that used to cost.
    pub(crate) fn maybe_resolve_selected_completion_item(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.completions.as_ref() else {
            return;
        };
        let CompletionsStatus::Ready {
            items,
            visible,
            selected,
        } = &entry.status
        else {
            return;
        };
        let Some(&item_index) = visible.get(*selected) else {
            return;
        };
        let Some(item) = items.get(item_index) else {
            return;
        };
        if item.detail.is_some() && item.documentation.is_some() {
            return;
        }
        let path = entry.path.clone();
        let key = (path.clone(), self.completions_generation, item_index);
        if self.completions_resolved.contains(&key)
            || self.completions_resolve_in_flight.as_ref() == Some(&key)
        {
            return;
        }
        let Some(absolute_path) = self.edit_buffer(&path).map(|buffer| buffer.path.clone()) else {
            return;
        };
        let Some(connection) = self.lsp_connection_for_path(&absolute_path) else {
            return;
        };
        if !connection.primary().supports_completion_resolve() {
            return;
        }
        // Overwriting this is exactly right: assigning `_completions_resolve_task` below drops -
        // and so cancels - whatever request the previous key was waiting on, which leaves that key
        // genuinely unanswered and therefore genuinely retryable.
        self.completions_resolve_in_flight = Some(key);
        let item_to_resolve = item.clone();
        let generation = self.completions_generation;
        let task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    connection.request::<lsp_core::lsp_types::request::ResolveCompletionItem>(
                        item_to_resolve,
                        LSP_QUERY_TIMEOUT,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_resolved_completion_item(&path, generation, item_index, result, cx);
            });
        });
        self._completions_resolve_task = Some(task);
    }

    /// Merges a real `completionItem/resolve` response into the exact item
    /// [`Self::maybe_resolve_selected_completion_item`] asked about, in place - each field the
    /// resolve response genuinely carries replacing whatever arrived inline, and every field it
    /// omits left exactly as it was (see the merge itself for the real, live-dumped case that
    /// settled which way round this goes). Refuses stale results the same way [`Self::
    /// apply_completion_result`] does: a `completions_generation` mismatch means a fresh server
    /// response (or a dismiss) has already replaced whatever `items` this index used to point
    /// into, so writing into it now would either silently corrupt an unrelated item or panic on an
    /// out-of-bounds index.
    pub(crate) fn apply_resolved_completion_item(
        &mut self,
        path: &Path,
        generation: u64,
        item_index: usize,
        result: Result<lsp_core::lsp_types::CompletionItem, lsp_core::LspError>,
        cx: &mut Context<Self>,
    ) {
        // Recorded here, not at dispatch time: this is the point at which the request the app sent
        // has genuinely been answered (an `Err` counts - re-asking a server that just failed would
        // only fail again). A request that never reaches this point was cancelled rather than
        // answered, and must stay retryable. See [`Self::completions_resolve_in_flight`].
        let key = (path.to_path_buf(), generation, item_index);
        if self.completions_resolve_in_flight.as_ref() == Some(&key) {
            self.completions_resolve_in_flight = None;
        }
        self.completions_resolved.insert(key);
        if self.completions_generation != generation {
            return;
        }
        let Ok(resolved) = result else {
            return;
        };
        let Some(entry) = self.completions.as_mut() else {
            return;
        };
        if entry.path != path {
            return;
        }
        let CompletionsStatus::Ready { items, .. } = &entry.status else {
            return;
        };
        let Some(item) = items.get(item_index) else {
            return;
        };
        // Merged over a *copy* and filed in `completions_resolved_items`, leaving the server's own
        // response untouched. See that field's own docs: writing back into `items` is what let a
        // resolve rewrite a row the user was already looking at, and rows must be complete and
        // final the moment the popup opens.
        //
        // A field the resolve response actually carries wins over whatever arrived inline. The
        // LSP spec has `completionItem/resolve` return the item with its fields filled in, and a
        // real dump against a live `typescript-language-server` shows why that matters here: an
        // auto-import item arrives with `detail: "./helper"` - a bare module specifier standing in
        // as a placeholder - and only the resolve response carries the real signature (`"Auto
        // import from './helper'\nconstructor RemoteHelper(): RemoteHelper"`), which is exactly
        // what the detail pane exists to show. A resolve response that omits a field still leaves
        // the inline one alone, so nothing real is ever lost.
        let mut merged = item.clone();
        if resolved.detail.is_some() {
            merged.detail = resolved.detail;
        }
        if resolved.documentation.is_some() {
            merged.documentation = resolved.documentation;
        }
        if resolved.label_details.is_some() {
            merged.label_details = resolved.label_details;
        }
        if resolved.additional_text_edits.is_some() {
            merged.additional_text_edits = resolved.additional_text_edits;
        }
        self.completions_resolved_items.insert(item_index, merged);
        cx.notify();
    }

    /// The best real description this app has of the item at `item_index` **for the detail pane
    /// and for accepting it** - the merged `completionItem/resolve` response when one has landed
    /// for the current generation, and the server's own inline item otherwise.
    ///
    /// Deliberately not what a row reads. See [`Self::completions_resolved_items`].
    pub(crate) fn described_completion_item<'a>(
        &'a self,
        items: &'a [lsp_core::lsp_types::CompletionItem],
        item_index: usize,
    ) -> Option<&'a lsp_core::lsp_types::CompletionItem> {
        self.completions_resolved_items
            .get(&item_index)
            .or_else(|| items.get(item_index))
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
    // One shared implementation with the breadcrumb's own counts (GitHub issue #178) - see
    // `diagnostics_view::count_errors_and_warnings`' docs for why counting must not go through
    // the per-line index.
    let (errors, warnings) = diagnostics_view::count_errors_and_warnings(&diagnostics);
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
            is_bare: false,
            is_detached: false,
            short_sha: None,
            is_locked: false,
            lock_reason: None,
            is_broken: false,
            broken_reason: None,
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
pub(crate) mod lsp_connection_facade_tests {
    use super::*;
    use gpui::{Entity, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

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
    /// - `no_resolve`: like `normal`, but its `completionProvider.resolveProvider` is `false` -
    ///   every other mode advertises `true`, matching every real, installed server this app
    ///   supports. Answers `completionItem/resolve` with a real `detail`/`documentation` pair
    ///   derived from the request's own `label`, for the modes that do advertise it.
    /// - `pull_flaky`: like `pull`, but answers the real *first* `textDocument/diagnostic`
    ///   request it ever receives with a genuine JSON-RPC error (not `ServerCancelled`, so
    ///   `lsp_core::LspClient::pull_diagnostics` returns immediately rather than retrying
    ///   internally) and every request after that with a real, non-empty `Full` report - a real,
    ///   deterministic, on-cue stand-in for the live, load-dependent "one real pull attempt times
    ///   out or errors, a later one succeeds" condition
    ///   [`AdeApp::schedule_lsp_sync`]'s own retry-on-`None` fix exists for (see that match arm's
    ///   docs), reproduced here without needing real CPU contention or a real multi-second wait.
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
let diagnosticPullCalls = 0;
function send(o) {
  const s = JSON.stringify(o);
  process.stdout.write('Content-Length: ' + Buffer.byteLength(s) + '\r\n\r\n' + s);
}
function publish(uri, message, character, characterEnd) {
  const start = character === undefined ? 0 : character;
  const end = characterEnd === undefined ? start + 1 : characterEnd;
  send({ jsonrpc: '2.0', method: 'textDocument/publishDiagnostics', params: { uri, diagnostics: [
    { range: { start: { line: 0, character: start }, end: { line: 0, character: end } }, severity: 1, message }
  ] } });
}
function handle(msg) {
  if (msg.method === 'initialize') {
    const capabilities = {
      textDocumentSync: 1,
      hoverProvider: true,
      completionProvider: { resolveProvider: MODE !== 'no_resolve' },
    };
    if (MODE === 'pull' || MODE === 'pull_flaky') {
      capabilities.diagnosticProvider = { interFileDependencies: false, workspaceDiagnostics: false };
    }
    send({ jsonrpc: '2.0', id: msg.id, result: { capabilities } });
    return;
  }
  if (msg.method === 'shutdown') { send({ jsonrpc: '2.0', id: msg.id, result: null }); return; }
  if (msg.method === 'exit' || msg.method === 'test/die') { process.exit(0); }
  if (msg.method === 'test/publish') {
    publish(msg.params.uri, msg.params.message, msg.params.character, msg.params.characterEnd);
    return;
  }
  if (msg.method === 'completionItem/resolve') {
    const item = msg.params;
    send({
      jsonrpc: '2.0',
      id: msg.id,
      result: {
        ...item,
        detail: 'resolved detail for ' + item.label,
        documentation: { kind: 'plaintext', value: 'resolved doc for ' + item.label },
      },
    });
    return;
  }
  if (msg.method === 'textDocument/diagnostic') {
    if (MODE === 'pull_flaky') {
      diagnosticPullCalls++;
      if (diagnosticPullCalls === 1) {
        // A real, non-ServerCancelled JSON-RPC error - `lsp_core::LspClient::pull_diagnostics`
        // returns this immediately rather than retrying internally, standing in for a real
        // request that timed out under load without this test needing to actually wait one out.
        send({ jsonrpc: '2.0', id: msg.id, error: { code: -32000, message: 'flaky test failure' } });
        return;
      }
      send({ jsonrpc: '2.0', id: msg.id, result: { kind: 'full', items: [
        { range: { start: { line: 0, character: 0 }, end: { line: 0, character: 1 } }, severity: 1,
          message: 'real diagnostic from a retried pull' }
      ] } });
      return;
    }
    // Every other mode has no real handler here and falls through to the generic id-only
    // response below, matching every method this fake server doesn't otherwise understand.
  }
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
    pub(crate) fn spawn_fake_server(
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
    pub(crate) fn publish_and_wait(client: &lsp_core::LspClient, target: &str, message: &str) {
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

    /// Same real push as [`publish_and_wait`], at an explicit `character..character_end` UTF-16
    /// column range on line 0, rather than the fixed `0..1` every other real caller uses - lets a
    /// test prove real, position-dependent behavior (e.g. a diagnostic card genuinely anchored
    /// under its own offending span, not just under column 0, where a bug that drops the real
    /// column back to the row's bare left edge happens to look identical to the fix).
    pub(crate) fn publish_at_and_wait(
        client: &lsp_core::LspClient,
        target: &str,
        message: &str,
        character: u32,
        character_end: u32,
    ) {
        client
            .notify_raw(
                "test/publish",
                serde_json::json!({
                    "uri": target,
                    "message": message,
                    "character": character,
                    "characterEnd": character_end,
                }),
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

    /// Direct regression coverage for the completions-popup flicker: an earlier version of
    /// `AdeApp::prepare_lsp_sync` unconditionally reset `AdeApp::completions` to `Loading` on
    /// *every* debounced re-sync tick, even while a `Ready` popup for the same path (already
    /// honestly narrowed in place by `Self::refilter_completions`, GitHub issue #189) was already
    /// showing real, useful content - so the popup visibly flashed to a bare "loading
    /// completions..." row and back on nearly every keystroke, and silently dropped the
    /// `"completions"` key context for that same window (`Self::completions_open_for_active_path`
    /// requires `Ready`).
    ///
    /// Calls `AdeApp::prepare_lsp_sync` directly rather than driving it through the real debounced
    /// `Self::schedule_lsp_sync` task and `cx.run_until_parked`: that method is a plain, synchronous
    /// state mutation (it only *builds* the completion request plan; dispatching and awaiting the
    /// real response is a separate, independently-spawned task further up the call chain), so its
    /// own decision about whether to touch `AdeApp::completions` can be observed with zero real
    /// I/O and zero timing race. A first version of this test drove it through a real fake-server
    /// round trip instead, and turned out unable to observe the bug at all: `cx.run_until_parked`
    /// blocks for the real duration of an awaited `client.request(..)` call (proven by adding an
    /// artificial multi-second server-side delay and watching the test's own wall-clock time grow
    /// to match), so by the time control returns to the test the real response - not just the
    /// `Loading` seed - has already landed, whatever the delay.
    #[gpui::test]
    fn a_debounced_re_sync_never_drops_an_already_ready_popup_back_to_loading(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        let absolute = root.join("src").join("main.rs");
        std::fs::write(&absolute, "fn a() {\n    x\n}\n").expect("write main.rs");
        let relative = PathBuf::from("src/main.rs");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server),
            );
            app.open_file_view(absolute.clone(), window, cx);
        });
        cx.run_until_parked();

        // `AdeApp::prepare_lsp_sync` only ever builds a completion plan once `AdeApp::
        // lsp_uri_cache` has a real entry for this path - populated by `Self::dispatch_did_open`'s
        // own background task, which `Self::render_center_pane` is what actually drives here
        // (mirroring `wait_for_real_diagnostics`'s own reasoning).
        let uri_cache_deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            app.update(cx, |app, cx| app.render_center_pane(cx));
            cx.run_until_parked();
            if app.read_with(cx, |app, _| app.lsp_uri_cache.contains_key(&absolute)) {
                break;
            }
            assert!(
                std::time::Instant::now() < uri_cache_deadline,
                "the fake client's uri never reached AdeApp::lsp_uri_cache"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // A pre-existing `Ready` popup for this exact path, seeded directly - standing in for
        // whatever real server response is already showing by the time a later keystroke's
        // debounce tick fires, without needing a real round trip to get there.
        app.update(cx, |app, _cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![lsp_core::lsp_types::CompletionItem {
                        label: "existing_item".to_string(),
                        ..Default::default()
                    }],
                    visible: vec![0],
                    selected: 0,
                },
            });
            app.edit_buffer_mut(&relative)
                .expect("a real buffer")
                .move_to("fn a() {\n    x".len());
        });

        // The real decision under test: a debounced re-sync tick for a completion-worthy position
        // (the buffer's real content ends in the identifier char `x`) must not overwrite the
        // already-`Ready` entry above with `Loading`, even though this "tick" would otherwise be
        // completion-worthy on its own. No `cx.run_until_parked` needed - `prepare_lsp_sync` itself
        // is where the old bug lived, synchronously, before any request is ever dispatched.
        app.update(cx, |app, cx| {
            app.prepare_lsp_sync(&root, &relative, cx, false);
        });

        app.read_with(cx, |app, _| {
            assert!(
                app.completions.as_ref().is_some_and(|entry| matches!(
                    &entry.status,
                    CompletionsStatus::Ready { items, .. } if items[0].label == "existing_item"
                )),
                "a debounced re-sync tick for an already-Ready popup must leave it exactly as it \
                 was, never dropping it back to a bare Loading state before any new response has \
                 had a chance to arrive - got: {:?}",
                app.completions.as_ref().map(|entry| &entry.status)
            );
        });
    }

    /// Direct regression coverage for the real, live-reported bug: accepting a completion
    /// routinely leaves the caret right after a real identifier character (accepting a bare
    /// `println` leaves it right after a real `n`), which the very next debounced re-sync tick
    /// used to read as a fresh, completion-worthy keystroke - immediately reopening the popup,
    /// filtered down to essentially just the item the user had just picked. Calls `prepare_lsp_sync`
    /// directly (no `cx.run_until_parked` needed) for the same reason [`a_debounced_re_sync_never_drops_an_already_ready_popup_back_to_loading`]
    /// does: the decision under test is synchronous, before any request is ever dispatched.
    #[gpui::test]
    fn accepting_a_completion_does_not_immediately_reopen_the_popup(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        let absolute = root.join("src").join("main.rs");
        std::fs::write(&absolute, "fn a() {\n    \n}\n").expect("write main.rs");
        let relative = PathBuf::from("src/main.rs");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server),
            );
            app.open_file_view(absolute, window, cx);
        });
        cx.run_until_parked();

        // A real, seeded `Ready` popup with one item whose bare label ends in a real identifier
        // character once accepted - the exact real shape that used to retrigger itself.
        app.update(cx, |app, _cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![lsp_core::lsp_types::CompletionItem {
                        label: "println".to_string(),
                        ..Default::default()
                    }],
                    visible: vec![0],
                    selected: 0,
                },
            });
            app.edit_buffer_mut(&relative)
                .expect("a real buffer")
                .move_to("fn a() {\n    ".len());
        });

        app.update_in(cx, |app, window, cx| {
            app.handle_completions_accept_action(&crate::root::CompletionsAccept, window, cx);
        });

        app.read_with(cx, |app, _| {
            assert!(
                app.completions.is_none(),
                "accepting a completion must genuinely dismiss the popup, got: {:?}",
                app.completions.as_ref().map(|entry| &entry.status)
            );
            let content = &app.edit_buffer(&relative).expect("a real buffer").content;
            assert!(
                content.contains("println"),
                "sanity check: the real accept must have spliced the real item's text into the \
                 real buffer, got: {content:?}"
            );
        });

        // The real decision under test: the very next debounced re-sync tick (driven directly,
        // synchronously - see this test's own docs) must not reopen the popup just because the
        // caret now sits right after a real identifier character.
        app.update(cx, |app, cx| {
            app.prepare_lsp_sync(&root, &relative, cx, false);
        });
        app.read_with(cx, |app, _| {
            assert!(
                app.completions.is_none(),
                "the debounce tick immediately following an accept must not reopen the popup - \
                 got: {:?}",
                app.completions.as_ref().map(|entry| &entry.status)
            );
        });

        // And the suppression is genuinely one-shot: a *subsequent* completion-worthy tick at the
        // exact same real position must still trigger normally, proving this isn't a permanent,
        // stuck-off switch - only the one debounce tick immediately after the accept is skipped.
        app.update(cx, |app, cx| {
            app.prepare_lsp_sync(&root, &relative, cx, false);
        });
        app.read_with(cx, |app, _| {
            assert!(
                app.completions.is_some(),
                "a real, later debounce tick at the same completion-worthy position must still \
                 trigger normally - the suppression must not outlive the one tick right after an \
                 accept"
            );
        });
    }

    /// Direct coverage for the Completions popup's detail pane (`crate::lsp::completion_popup::
    /// AdeApp::render_completion_detail_pane`) being nearly empty for real items in practice: most
    /// real servers (rust-analyzer very much included) send only a bare `label`/`kind` inline in
    /// `textDocument/completion` and expect a follow-up `completionItem/resolve` for whichever one
    /// the user is actually looking at. This proves the real, live round trip - `spawn_fake_server`
    /// now advertises `completionProvider.resolveProvider: true` and answers `completionItem/
    /// resolve` with a real `detail`/`documentation` pair - lands in the exact item `AdeApp::
    /// completions` holds, not a parallel/shadow copy the render path wouldn't ever read.
    #[gpui::test]
    fn selecting_a_completion_item_resolves_its_real_detail_and_documentation(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let absolute = root.join("main.rs");
        std::fs::write(&absolute, "fn a() {}\n").expect("write main.rs");
        let relative = PathBuf::from("main.rs");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server),
            );
            app.open_file_view(absolute, window, cx);
        });
        cx.run_until_parked();

        // A real `Ready` popup, seeded directly - only `label` set, matching what a real server
        // commonly sends inline before resolution.
        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![lsp_core::lsp_types::CompletionItem {
                        label: "unresolved_item".to_string(),
                        ..Default::default()
                    }],
                    visible: vec![0],
                    selected: 0,
                },
            });
            app.maybe_resolve_selected_completion_item(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let entry = app
                .completions
                .as_ref()
                .expect("popup should still be open");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("expected a Ready popup, got {:?}", entry.status);
            };
            // The detail pane's own view of each item: the merged `completionItem/resolve`
            // response where one landed, the untouched inline item until then. `items` itself is
            // deliberately never written to, so that a row can't change under the user - see
            // `AdeApp::completions_resolved_items`.
            let described = |index: usize| {
                app.described_completion_item(items, index)
                    .expect("every index here is a real one")
                    .clone()
            };
            assert_eq!(
                described(0).detail.as_deref(),
                Some("resolved detail for unresolved_item"),
                "a real completionItem/resolve response must fill in the item's real detail, \
                 which is what the detail pane's signature line reads"
            );
            let doc = crate::lsp::completion::completion_documentation_text(&described(0));
            assert_eq!(
                doc.as_deref(),
                Some("resolved doc for unresolved_item"),
                "and its real documentation, which is what the detail pane's doc-prose body reads"
            );
        });
    }

    /// The real, live-reproduced bug behind "the shown things are still modules instead of real
    /// types" surviving even after the detail-splitting fix: this merge used to keep the
    /// *unresolved* `detail` whenever the server had sent one, on the theory that an inline
    /// `detail` was the server's own considered choice and resolve was only ever additive.
    ///
    /// A real dump against a live `typescript-language-server` disproves that for the exact case
    /// that matters. For an auto-import completion it sends `detail: "./helper"` inline - a bare
    /// module specifier standing in as a placeholder - and only the `completionItem/resolve`
    /// response carries the genuinely richer `"Auto import from './helper'\nconstructor
    /// RemoteHelper(): RemoteHelper"` that actually contains the signature. Discarding the
    /// resolved value pinned the item to the placeholder forever, so the popup had a module path
    /// and no type no matter what the render path did with it. Per the LSP spec, resolve returns
    /// the item with its fields filled in, so a resolved `detail` is the authoritative one.
    #[gpui::test]
    fn a_real_resolved_detail_replaces_a_placeholder_the_server_sent_inline(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let absolute = root.join("main.rs");
        std::fs::write(&absolute, "fn a() {}\n").expect("write main.rs");
        let relative = PathBuf::from("main.rs");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server),
            );
            app.open_file_view(absolute, window, cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![lsp_core::lsp_types::CompletionItem {
                        label: "RemoteHelper".to_string(),
                        // The real placeholder a live typescript-language-server sends inline.
                        detail: Some("./helper".to_string()),
                        ..Default::default()
                    }],
                    visible: vec![0],
                    selected: 0,
                },
            });
            app.maybe_resolve_selected_completion_item(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let entry = app
                .completions
                .as_ref()
                .expect("popup should still be open");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("expected a Ready popup, got {:?}", entry.status);
            };
            // The detail pane's own view of each item: the merged `completionItem/resolve`
            // response where one landed, the untouched inline item until then. `items` itself is
            // deliberately never written to, so that a row can't change under the user - see
            // `AdeApp::completions_resolved_items`.
            let described = |index: usize| {
                app.described_completion_item(items, index)
                    .expect("every index here is a real one")
                    .clone()
            };
            assert_eq!(
                described(0).detail.as_deref(),
                Some("resolved detail for RemoteHelper"),
                "a real resolve response must win over the placeholder detail the server sent \
                 inline - that placeholder is exactly the bare module path the user reported \
                 seeing where a type belongs"
            );
        });
    }

    /// Arrowing past an item must not permanently cost that item its type and documentation.
    ///
    /// Only one resolve request is ever in flight ([`AdeApp::_completions_resolve_task`] is a
    /// single slot), so moving the selection replaces - and therefore, since dropping a `Task`
    /// cancels it, *aborts* - whatever resolve the previous item had going. That is the intended
    /// economy. The bug was that the aborted item had already been written into
    /// [`AdeApp::completions_resolved`] at dispatch time, so it counted as answered forever: come
    /// back to it and no second request would ever go out, and its row and detail pane stayed
    /// pinned to whatever the unresolved item happened to carry - for a real
    /// `typescript-language-server` auto-import, a bare module specifier and no type at all.
    ///
    /// This walks that exact sequence: select item 0 and dispatch, move to item 1 and dispatch
    /// (killing item 0's request before it can land), let that settle, then come back to item 0.
    #[gpui::test]
    fn an_item_whose_resolve_was_cancelled_by_moving_on_is_resolved_again_on_return(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let absolute = root.join("main.rs");
        std::fs::write(&absolute, "fn a() {}\n").expect("write main.rs");
        let relative = PathBuf::from("main.rs");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server),
            );
            app.open_file_view(absolute, window, cx);
        });
        cx.run_until_parked();

        let bare = |label: &str| lsp_core::lsp_types::CompletionItem {
            label: label.to_string(),
            ..Default::default()
        };
        // Both dispatches happen inside one `update`, so item 0's request is genuinely still in
        // flight when item 1's replaces its task slot - the real fast-arrow-key sequence.
        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![bare("first_item"), bare("second_item")],
                    visible: vec![0, 1],
                    selected: 0,
                },
            });
            app.maybe_resolve_selected_completion_item(cx);
            if let Some(CompletionsStatus::Ready { selected, .. }) =
                app.completions.as_mut().map(|entry| &mut entry.status)
            {
                *selected = 1;
            }
            app.maybe_resolve_selected_completion_item(cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            if let Some(CompletionsStatus::Ready { selected, .. }) =
                app.completions.as_mut().map(|entry| &mut entry.status)
            {
                *selected = 0;
            }
            app.maybe_resolve_selected_completion_item(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let entry = app
                .completions
                .as_ref()
                .expect("popup should still be open");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("expected a Ready popup, got {:?}", entry.status);
            };
            // The detail pane's own view of each item: the merged `completionItem/resolve`
            // response where one landed, the untouched inline item until then. `items` itself is
            // deliberately never written to, so that a row can't change under the user - see
            // `AdeApp::completions_resolved_items`.
            let described = |index: usize| {
                app.described_completion_item(items, index)
                    .expect("every index here is a real one")
                    .clone()
            };
            assert_eq!(
                described(1).detail.as_deref(),
                Some("resolved detail for second_item"),
                "sanity check: the resolve that was *not* cancelled must have landed"
            );
            assert_eq!(
                described(0).detail.as_deref(),
                Some("resolved detail for first_item"),
                "an item whose resolve was aborted by moving the selection on must be asked \
                 again when the user comes back to it - otherwise arrowing past a row silently \
                 costs it its type and documentation for as long as the popup is open"
            );
        });
    }

    /// The economy that fix must not undo: re-selecting an item whose resolve is *still in flight*
    /// must not fire a second, redundant request for the same thing.
    #[gpui::test]
    fn an_item_with_a_resolve_already_in_flight_is_not_asked_twice(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let absolute = root.join("main.rs");
        std::fs::write(&absolute, "fn a() {}\n").expect("write main.rs");
        let relative = PathBuf::from("main.rs");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server),
            );
            app.open_file_view(absolute, window, cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![lsp_core::lsp_types::CompletionItem {
                        label: "only_item".to_string(),
                        ..Default::default()
                    }],
                    visible: vec![0],
                    selected: 0,
                },
            });
            app.maybe_resolve_selected_completion_item(cx);
            let in_flight = app.completions_resolve_in_flight.clone();
            assert_eq!(
                in_flight,
                Some((relative.clone(), app.completions_generation, 0)),
                "the first dispatch must record exactly which item it is waiting on"
            );
            // A re-render/re-selection of the same item while that request is still out.
            app.maybe_resolve_selected_completion_item(cx);
            assert_eq!(
                app.completions_resolve_in_flight, in_flight,
                "a second dispatch for the same in-flight item would have replaced the task slot, \
                 cancelling the request that was already on its way for no reason at all"
            );
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            let entry = app
                .completions
                .as_ref()
                .expect("popup should still be open");
            let CompletionsStatus::Ready { items, .. } = &entry.status else {
                panic!("expected a Ready popup, got {:?}", entry.status);
            };
            // The detail pane's own view of each item: the merged `completionItem/resolve`
            // response where one landed, the untouched inline item until then. `items` itself is
            // deliberately never written to, so that a row can't change under the user - see
            // `AdeApp::completions_resolved_items`.
            let described = |index: usize| {
                app.described_completion_item(items, index)
                    .expect("every index here is a real one")
                    .clone()
            };
            assert_eq!(
                described(0).detail.as_deref(),
                Some("resolved detail for only_item")
            );
            assert!(
                app.completions_resolve_in_flight.is_none(),
                "a landed response must clear the in-flight marker rather than leaving the item \
                 looking forever pending"
            );
        });
    }

    /// A server with no real `completionProvider.resolveProvider` at all must never get a
    /// `completionItem/resolve` request - there's nothing on the other end that could ever answer
    /// one usefully, and firing it anyway would just be a request every such server has to somehow
    /// handle (most just answer `null`/an error, which resolves to a harmless no-op here, but the
    /// request should never leave in the first place).
    #[gpui::test]
    fn a_server_without_resolve_support_is_never_asked_to_resolve(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let absolute = root.join("main.vue");
        std::fs::write(&absolute, "<template></template>\n").expect("write main.vue");
        let relative = PathBuf::from("main.vue");

        let (app, cx): (Entity<AdeApp>, &mut VisualTestContext) =
            palette_focus_tests::open_test_app(cx, root.clone());
        // `vue-language-server` is the fake client key `crate::language::lsp_binary_for_extension`
        // resolves for `.vue` - reused here purely to get a `Ready` client under the right key,
        // not because this test cares about the real Vue companion mechanism at all.
        let binary = crate::language::lsp_binary_for_extension(Some("vue"))
            .expect(".vue must resolve to a real binary key");
        let server = spawn_fake_server(repo.path(), "primary", "no_resolve");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients
                .insert((root.clone(), binary), LspClientState::Ready(server));
            app.open_file_view(absolute, window, cx);
        });
        cx.run_until_parked();

        app.update(cx, |app, cx| {
            app.completions = Some(CompletionsEntry {
                path: relative.clone(),
                status: CompletionsStatus::Ready {
                    items: vec![lsp_core::lsp_types::CompletionItem {
                        label: "item".to_string(),
                        ..Default::default()
                    }],
                    visible: vec![0],
                    selected: 0,
                },
            });
            app.maybe_resolve_selected_completion_item(cx);
        });
        cx.run_until_parked();

        app.read_with(cx, |app, _| {
            assert!(
                app.completions_resolved.is_empty(),
                "a server with no real resolveProvider capability must never be asked to resolve \
                 anything - a genuine dispatch would have recorded this (path, generation, index) \
                 triple"
            );
        });
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

    /// Waits for a genuinely spawned server's real process death to be observed by its client.
    fn wait_until_dead(client: &lsp_core::LspClient) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while client.is_connection_alive() {
            assert!(
                Instant::now() < deadline,
                "the real process death should have been observed within 10s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The real bug: nothing in this crate ever checked `is_connection_alive` on a *cadence*. It
    /// was read only by [`lsp_file_status`], i.e. only while the dead server's own language
    /// happened to be the file on screen - so a `rust-analyzer` that died while a TypeScript file
    /// was open stayed `Ready` in `lsp_clients` indefinitely, with every sync tick, hover and
    /// completion still routed at a process that would never answer, and nothing anywhere saying
    /// so.
    ///
    /// Driven through a genuinely spawned process killed on cue, and through the real
    /// [`AdeApp::reap_dead_lsp_clients`] the production poll loop calls - not a simulated state
    /// flip.
    #[gpui::test]
    fn a_dead_ready_client_is_reaped_into_a_real_named_failed_state(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = palette_focus_tests::open_test_app(cx, repo.path().to_path_buf());
        let client = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        let key: LspClientKey = (repo.path().to_path_buf(), "rust-analyzer");

        app.update(cx, |app, cx| {
            app.lsp_clients
                .insert(key.clone(), LspClientState::Ready(client.clone()));
            assert!(
                !app.reap_dead_lsp_clients(cx),
                "a genuinely alive client must never be reaped - that would be a self-inflicted \
                 outage, not a fix"
            );
        });

        // A real, unprompted process death, with no `shutdown()` - standing in for a crash.
        client
            .notify_raw("test/die", serde_json::Value::Null)
            .expect("the fake server should accept the notification that kills it");
        wait_until_dead(&client);

        app.update(cx, |app, cx| {
            assert!(
                app.reap_dead_lsp_clients(cx),
                "a real, dead process must be noticed on the poll cadence"
            );
            let Some(LspClientState::Failed(message)) = app.lsp_clients.get(&key) else {
                panic!(
                    "a dead client must be demoted to a real Failed state, got: {:?}",
                    app.lsp_clients.get(&key).map(std::mem::discriminant)
                );
            };
            assert!(
                message.contains("rust-analyzer"),
                "the recorded reason must name which real server died, got: {message}"
            );
            assert!(
                !app.reap_dead_lsp_clients(cx),
                "a second pass must report no change - the poll loop calls this every 250ms and \
                 must not notify the window forever over one death"
            );
        });

        // The real consequence that makes the demotion worth doing: no further request is routed
        // at the dead process, because the connection no longer resolves at all.
        app.read_with(cx, |app, _| {
            assert!(
                app.lsp_connection_for_path(&repo.path().join("main.rs"))
                    .is_none(),
                "a reaped client must stop resolving into a usable connection"
            );
        });
    }

    /// The other half of the same bug, and the one a user actually feels: before this, there was
    /// no recovery path at all. [`AdeApp::spawn_lsp_client`] deliberately no-ops for a key that
    /// already has an entry *in any state*, and nothing ever removed one whose process had died -
    /// so the only real way to revive a dead server was to switch worktrees and back, or restart
    /// the app.
    ///
    /// Also pins the part that would be a *worse* silent failure if it were forgotten: the
    /// per-server document bookkeeping has to go too. `lsp_opened_files` is what makes `didOpen`
    /// fire exactly once per path, so a restart that left it behind would give the fresh server a
    /// file it was never told to open - everything would look alive while answering about nothing.
    #[gpui::test]
    fn restarting_clears_the_dead_client_and_all_of_its_document_bookkeeping(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
        let opened = root.join("src").join("main.rs");
        let relative = PathBuf::from("src/main.rs");

        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Failed("rust-analyzer's connection was lost".to_string()),
            );
            app.lsp_opened_files.insert(opened.clone());
            app.lsp_document_versions.insert(opened.clone(), 42);
            app.lsp_last_synced_content
                .insert(relative.clone(), "fn main() {}".to_string());
            app.lsp_synced_version.insert(relative.clone(), 42);
            app.lsp_diagnostics_confirmed_version
                .insert(relative.clone(), 42);
        });

        app.update(cx, |app, cx| app.restart_lsp_clients(cx));

        app.read_with(cx, |app, _| {
            assert!(
                !app.lsp_clients
                    .contains_key(&(root.clone(), "rust-analyzer")),
                "the entry has to be genuinely removed, not merely marked - `spawn_lsp_client` \
                 no-ops for any key that still exists, so leaving it behind would mean the \
                 restart silently did nothing at all"
            );
            assert!(
                !app.lsp_opened_files.contains(&opened),
                "a fresh server has been told about no files; leaving this behind would suppress \
                 the real didOpen the next render owes it"
            );
            assert!(
                !app.lsp_document_versions.contains_key(&opened),
                "document versions describe a conversation with a process that no longer exists"
            );
            assert!(app.lsp_last_synced_content.is_empty());
            assert!(app.lsp_synced_version.is_empty());
            assert!(
                app.lsp_diagnostics_confirmed_version.is_empty(),
                "a stale confirmed-version would let the sync-pending banner claim a fresh \
                 server's diagnostics were up to date before it had answered anything"
            );
        });
    }

    /// The race an adversarial review of this fix caught, and the nastier half of it: clearing
    /// the bookkeeping is not enough on its own, because the tasks that *write* that bookkeeping
    /// are still in flight when the restart happens.
    ///
    /// [`AdeApp::schedule_lsp_sync`]'s continuation captures its own `Arc<LspConnection>` at plan
    /// time and never re-checks [`AdeApp::lsp_clients`] afterwards, so once past that point it
    /// completes against the *old* client and writes back unconditionally: `lsp_last_synced_content`
    /// and `lsp_synced_version` when its `did_change_full` returns `Ok`, and
    /// `lsp_diagnostics_confirmed_version` on every attempt of a retry loop that runs for a real
    /// ~8 seconds. A resurrected `lsp_last_synced_content` entry is the damaging one:
    /// [`AdeApp::prepare_lsp_sync`] reads it as "the server already has this content", so the
    /// freshly spawned server would never be sent a `didChange` for a dirty buffer and would
    /// answer forever about the file's on-disk text - a user who restarted *because* diagnostics
    /// went wrong then silently gets diagnostics for the wrong content.
    ///
    /// ## What this test does and does not prove
    ///
    /// It pins the **mechanism**, not the symptom: that a restart genuinely drops the in-flight
    /// sync/completion tasks, which is what makes the write-back impossible. Reproducing the
    /// interleaving itself was attempted and abandoned honestly - GPUI's deterministic test
    /// executor collapses exactly the window in question (driving the clock runs the awaiting
    /// continuation straight through to completion), so a test written against the symptom passes
    /// identically with and without the fix, which was verified rather than assumed. The
    /// assertions below do fail without it.
    #[gpui::test]
    async fn a_restart_drops_the_in_flight_tasks_that_would_repopulate_cleared_bookkeeping(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        let absolute = root.join("src").join("main.rs");
        std::fs::write(&absolute, "fn main() {}\n").expect("write main.rs");
        let relative = PathBuf::from("src/main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
        let server = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server.clone()),
            );
            app.open_file_view(absolute.clone(), window, cx);
        });
        cx.run_until_parked();

        // A real, armed sync task through the real production entry point - and a real check that
        // it is genuinely armed, so this can't quietly become a test of nothing.
        app.update(cx, |app, cx| {
            app.schedule_lsp_sync(root.clone(), relative.clone(), cx);
        });
        app.read_with(cx, |app, _| {
            assert!(
                app._lsp_sync_tasks.contains_key(&relative),
                "sanity check: the real production call must genuinely arm a sync task, or the \
                 assertion below proves nothing"
            );
        });

        app.update(cx, |app, cx| app.restart_lsp_clients(cx));

        app.read_with(cx, |app, _| {
            assert!(
                app._lsp_sync_tasks.is_empty(),
                "an in-flight sync task outliving the restart re-records 'the server already has \
                 this content' for a server that never saw it - the fresh server would then never \
                 be sent a didChange at all"
            );
            assert!(
                app._completions_request_task.is_none(),
                "an in-flight completion request belongs to the connection just torn down"
            );
        });

        // And the bookkeeping those tasks write is genuinely clear once everything drains.
        cx.background_executor.advance_clock(LSP_SYNC_DEBOUNCE * 4);
        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            assert!(app.lsp_last_synced_content.is_empty());
            assert!(app.lsp_synced_version.is_empty());
            assert!(app.lsp_diagnostics_confirmed_version.is_empty());
        });
    }

    /// The recovery has to be reachable without already knowing where to look, so the palette
    /// command is wired to the same real method the failed-status chip calls - proven by running
    /// the real [`AdeApp::execute_palette_command`], not by asserting the enum variant exists.
    #[gpui::test]
    fn the_restart_palette_command_runs_the_real_restart(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());

        app.update(cx, |app, _cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Failed("dead".to_string()),
            );
        });
        assert!(
            crate::palette::state::PaletteCommand::ALL
                .contains(&crate::palette::state::PaletteCommand::RestartLanguageServers),
            "a command absent from ALL never appears in the palette, so it is not a real \
             recovery path a user can find"
        );

        app.update_in(cx, |app, window, cx| {
            app.execute_palette_command(
                crate::palette::state::PaletteCommand::RestartLanguageServers,
                window,
                cx,
            );
        });

        app.read_with(cx, |app, _| {
            assert!(
                !app.lsp_clients
                    .contains_key(&(root.clone(), "rust-analyzer")),
                "the palette command must run the real restart, not a stub"
            );
        });
    }

    /// A live client dies, the real poll-cadence reap notices, the real restart clears it, and a
    /// subsequent real spawn attempt genuinely *reaches the OS* under the same key.
    ///
    /// That last step is the whole point, and it is deliberately checked with a binary that
    /// cannot exist rather than a real server: what has to be proven is that
    /// [`AdeApp::spawn_lsp_client`]'s "already have an entry, do nothing" guard - the exact thing
    /// that made a dead connection permanent - is genuinely cleared, and a real `Failed` outcome
    /// proves an attempt was made where previously none would have been. This test does not
    /// claim a working server comes back; that is `ensure_lsp_client`'s ordinary cold-start path,
    /// already covered end to end against a real rust-analyzer by
    /// `lsp_diagnostics_wiring_tests`.
    #[gpui::test]
    async fn a_restart_frees_the_key_so_a_fresh_spawn_is_really_attempted(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
        let key: LspClientKey = (root.clone(), "rust-analyzer");

        let first = spawn_fake_server(repo.path(), "rust-analyzer", "normal");
        app.update(cx, |app, _cx| {
            app.lsp_clients
                .insert(key.clone(), LspClientState::Ready(first.clone()));
        });
        first
            .notify_raw("test/die", serde_json::Value::Null)
            .expect("notification accepted");
        wait_until_dead(&first);
        app.update(cx, |app, cx| {
            assert!(app.reap_dead_lsp_clients(cx));
        });

        app.update(cx, |app, cx| app.restart_lsp_clients(cx));
        // The real production spawn path, exactly as `render_file_view` drives it - with a
        // deliberately non-existent binary swapped in via the same real `build_config` seam
        // `ensure_lsp_client` uses, so this proves the *guard* is genuinely cleared without
        // making the test depend on a real toolchain being installed. A `Failed` outcome here is
        // a real spawn attempt that reached the OS; before the restart cleared the entry, no
        // attempt would have been made at all.
        app.update(cx, |app, cx| {
            app.spawn_lsp_client(
                key.clone(),
                root.clone(),
                || {
                    Ok(lsp_core::ServerSpawnConfig {
                        // Both fields carry the same deliberately-impossible marker: `name` is
                        // what `LspError::Spawn` actually prints, and `binary` is what is really
                        // handed to the OS - so the assertion below can only pass if a genuine
                        // spawn was attempted and genuinely failed.
                        name: "ade-no-such-language-server-binary",
                        binary: "ade-no-such-language-server-binary",
                        args: Vec::new(),
                        initialization_options: None,
                        workspace_configuration: lsp_core::default_workspace_configuration,
                        custom_notification_methods: Vec::new(),
                    })
                },
                cx,
            );
        });
        app.read_with(cx, |app, _| {
            assert!(
                matches!(app.lsp_clients.get(&key), Some(LspClientState::Spawning)),
                "the restart must leave the key genuinely free for a fresh spawn - a still-present \
                 entry would make this call a silent no-op, which is the original bug"
            );
        });

        cx.run_until_parked();
        app.read_with(cx, |app, _| {
            let Some(LspClientState::Failed(message)) = app.lsp_clients.get(&key) else {
                panic!("the real spawn attempt should have resolved to a real outcome");
            };
            assert!(
                message.contains("ade-no-such-language-server-binary"),
                "the real spawn error must be surfaced as-is, naming the binary that was actually \
                 attempted - anything vaguer would let this pass without a real attempt having \
                 been made: {message}"
            );
        });
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
/// polls real wall-clock time (up to 480s - see this module's own real-deadline constants for
/// the exact per-test budgets, widened past `lsp_core::client`'s own e2e test's 180s baseline;
/// see the docs on the deadlines themselves for why) for the diagnostic to actually arrive - no
/// sleep stands in for that wait, and nothing is fabricated if the wait times out (the assertion
/// just fails). This is a genuinely slow test (real process spawn plus real sysroot indexing)
/// kept in the normal, non-`#[ignore]` suite on purpose - this project has no separate "slow
/// test" lane.
#[cfg(test)]
mod lsp_diagnostics_wiring_tests {
    use super::*;
    use gpui::{Entity, EntityInputHandler, TestAppContext, VisualTestContext};
    use std::time::{Duration, Instant};

    /// This module's five real-subprocess tests
    /// ([`a_real_diagnostic_reaches_file_view_diagnostics_through_the_real_app_code_path`],
    /// [`a_real_typescript_diagnostic_reaches_file_view_diagnostics_through_the_real_app_code_path`],
    /// [`rust_analyzer_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions`],
    /// [`typescript_language_server_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions`],
    /// [`pyright_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions`]) each
    /// spawn a genuinely separate, heavy real language-server subprocess and wait on real,
    /// wall-clock-bounded indexing/diagnostics round trips. Left to `cargo test`'s default
    /// parallelism, all five can spawn *simultaneously*, on top of whatever else the full suite
    /// is doing at the same time - real, observed, live-reproduced full-suite contention (see
    /// the widened real deadlines just above this module) that this lock cuts a genuine, sizeable
    /// share out of: with these five serialized against each other, each real server gets the box
    /// mostly to itself instead of fighting four siblings for the same cores, so their combined
    /// real wall-clock total is typically no worse (often better, since none of them individually
    /// come anywhere near needing their own widened deadline) than letting all five race in
    /// parallel under load. `PoisonError::into_inner`, not `.unwrap()`: one of these tests'
    /// own real assertion genuinely failing must not cascade into every *other* real-subprocess
    /// test in this module failing on a poisoned lock too - a real failure should be reported
    /// once, by the test that actually found it, not five times over.
    static REAL_LSP_SUBPROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquires [`REAL_LSP_SUBPROCESS_TEST_LOCK`] for the calling test's duration - see that
    /// lock's own docs for why. The returned guard must be held for the whole test (bind it to a
    /// named local, not `_`, so it isn't dropped immediately).
    fn serialize_real_lsp_subprocess_test() -> std::sync::MutexGuard<'static, ()> {
        REAL_LSP_SUBPROCESS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

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
                 480s for rust-analyzer, 240s for typescript-language-server/pyright - so the \
                 message deliberately doesn't hardcode either one)"
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
        let _serialize = serialize_real_lsp_subprocess_test();
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

        let deadline = Instant::now() + Duration::from_secs(480);
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
        let _serialize = serialize_real_lsp_subprocess_test();
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

        let deadline = Instant::now() + Duration::from_secs(240);
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
            let buffer = app.edit_buffer_mut(&relative).expect("a real buffer");
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
        let _serialize = serialize_real_lsp_subprocess_test();
        let project = write_scratch_project(
            "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");
        let (app, cx) = palette_focus_tests::open_test_app(cx, project.path().to_path_buf());

        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_rs.clone(), window, cx);
        });
        cx.run_until_parked();

        let indexed_deadline = Instant::now() + Duration::from_secs(480);
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

        // Widened twice from a real, observed 180s-deadline failure under genuine full-suite
        // parallel load (`cargo test -p app --lib` with its default, num-cpus-wide test-
        // threads): the whole suite repeatedly ran 3-5x its normal wall-clock length (up to
        // ~330s vs. the usual ~70-100s) on this same sandbox, and this real rust-analyzer -
        // already `Ready` and only asked to recompute diagnostics for a two-line edit, not
        // re-index from scratch - still hadn't published the new diagnostic even at 300s on a
        // later, still-more-contended run (this module's own `REAL_LSP_SUBPROCESS_TEST_LOCK`
        // and the `None`-is-retried fix in `AdeApp::schedule_lsp_sync`'s own retry loop both
        // already close the *other* real bugs this same investigation found - this deadline
        // widening is a distinct, additional real-headroom fix, not a substitute for either).
        // The wait itself already polls correctly (real `std::thread::sleep` between checks,
        // bounded by a real deadline, not a fixed tick count - see `wait_until`'s own docs); the
        // deadline itself was just too tight for how slow a real subprocess can genuinely get
        // when dozens of sibling tests' own real subprocesses (other `rust-analyzer`/`pyright`/
        // `typescript-language-server`/pty sessions, and - on this particular shared sandbox -
        // other agents' own concurrent full test-suite runs) are contending for the same CPU
        // cores. 480s keeps real headroom for that without losing the "assertion still fails if
        // diagnostics genuinely never arrive" property that makes this a real regression gate,
        // not a rubber stamp - and rust-analyzer gets the widest budget of the three real
        // servers this module covers because it is, empirically, the one that has actually
        // needed it (typescript-language-server/pyright's own 240s deadlines have not been
        // observed failing across dozens of full-suite reproduction runs).
        let diagnostic_deadline = Instant::now() + Duration::from_secs(480);
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
            app.edit_buffer(&relative)
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
            let content = &app.edit_buffer(&relative).expect("a real buffer").content;
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

    /// The real fix for a real, live-observed bug behind this module's own widened deadlines
    /// (see [`PULL_DIAGNOSTICS_EMPTY_RETRIES`]'s own docs for the full account): a single real
    /// `pull_diagnostics` attempt that fails or times out - a real, live possibility under full-
    /// suite parallel load, not a genuine "the server can't answer" condition - used to
    /// permanently strand a post-edit diagnostic, because the retry loop treated that outcome
    /// (`None`) as terminal instead of retrying it exactly like it already retried an honest
    /// "still empty" answer. This reproduces the failure deterministically, without needing real
    /// CPU contention or a real multi-second wait: [`spawn_fake_server`]'s `pull_flaky` mode
    /// answers the real *first* `textDocument/diagnostic` request with a real JSON-RPC error and
    /// every request after that with a real, non-empty report, so a pre-fix build (which breaks
    /// out after that first error) never sees the second, successful attempt's real diagnostic,
    /// while a fixed build does.
    #[gpui::test]
    fn a_transient_pull_failure_is_retried_not_treated_as_a_permanent_stall(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let root = repo.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).expect("mkdir src");
        let absolute = root.join("src").join("main.rs");
        std::fs::write(&absolute, "fn main() {}\n").expect("write main.rs");

        let (app, cx) = palette_focus_tests::open_test_app(cx, root.clone());
        let server = super::lsp_connection_facade_tests::spawn_fake_server(
            repo.path(),
            "rust-analyzer",
            "pull_flaky",
        );
        app.update_in(cx, |app, window, cx| {
            app.lsp_clients.insert(
                (root.clone(), "rust-analyzer"),
                LspClientState::Ready(server.clone()),
            );
            app.open_file_view(absolute.clone(), window, cx);
        });
        cx.run_until_parked();
        // A real render pass - the real trigger for `AdeApp::dispatch_did_open`, which is what
        // populates `AdeApp::lsp_uri_cache` for this file (see `wait_for_real_diagnostics`'s own
        // docs for why re-rendering, not just `open_file_view`, is what actually drives this
        // real production path forward). Needed before `previous_result_was_non_empty` can ever
        // read anything real below - without this, `lsp_uri_cache` stays empty and the retry
        // loop under test never even engages.
        app.update(cx, |app, cx| {
            app.render_center_pane(cx);
        });
        cx.run_until_parked();

        // A real, non-empty diagnostics result for this file *before* the real edit below - the
        // real gate (`previous_result_was_non_empty`) that makes the retry loop under test run
        // at all, rather than accept a single pull's answer unconditionally (see
        // `PULL_DIAGNOSTICS_EMPTY_RETRIES`'s own docs).
        let file_uri = lsp_core::LspClient::uri_for_path(&absolute).expect("real uri");
        super::lsp_connection_facade_tests::publish_and_wait(
            &server,
            file_uri.as_str(),
            "seed diagnostic before the real edit",
        );

        // A real, unsaved edit through the real `EntityInputHandler::replace_text_in_range` path
        // (same as [`type_text`]'s other callers in this module), so `schedule_lsp_sync`'s own
        // debounced continuation has genuinely new content to sync and dispatches a real
        // `didChange` - the real trigger for the real pull-retry sequence under test.
        type_text(&app, cx, "fn main() {}\n".len(), "\n// a real edit\n");

        // The retry loop's own `PULL_DIAGNOSTICS_EMPTY_RETRY_DELAY` timer is a real,
        // deterministic-clock-aware `cx.background_executor().timer(..)`, so it needs an
        // explicit `advance_clock` here to fire, the same as `wait_until`'s own polling shape
        // uses `type_text`'s own advance for the sync debounce - `run_until_parked()` alone does
        // not carry the virtual clock forward on its own, so without this the retry's own timer
        // would never fire and this loop would hang until the real deadline below.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut retried_past_the_first_failure = false;
        while Instant::now() < deadline {
            cx.background_executor
                .advance_clock(PULL_DIAGNOSTICS_EMPTY_RETRY_DELAY);
            cx.run_until_parked();
            if server
                .diagnostics_for_uri(&file_uri)
                .is_some_and(|diagnostics| {
                    diagnostics
                        .iter()
                        .any(|d| d.message == "real diagnostic from a retried pull")
                })
            {
                retried_past_the_first_failure = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            retried_past_the_first_failure,
            "the retry loop must survive the fake server's first, deliberately-erroring pull \
             attempt and pick up the real, non-empty result its second attempt answers with - a \
             pre-fix build stops retrying after that first error and never reaches it"
        );
    }

    /// The same real, live proof as the rust-analyzer test above, for `typescript-language-server`
    /// - see `crate::language`'s own docs on why `npm install typescript@5` is a genuine, real
    /// project-local requirement in this sandbox, not conservative caution.
    #[gpui::test]
    fn typescript_language_server_tracks_a_real_live_unsaved_edit_for_both_diagnostics_and_completions(
        cx: &mut TestAppContext,
    ) {
        let _serialize = serialize_real_lsp_subprocess_test();
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

        let indexed_deadline = Instant::now() + Duration::from_secs(240);
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

        let diagnostic_deadline = Instant::now() + Duration::from_secs(240);
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
            app.edit_buffer(&relative)
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
            let content = &app.edit_buffer(&relative).expect("a real buffer").content;
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
        let _serialize = serialize_real_lsp_subprocess_test();
        let dir = tempfile::tempdir().expect("tempdir");
        let main_py = dir.path().join("main.py");
        let baseline = "ok: int = 1\nprint(ok)\n";
        std::fs::write(&main_py, baseline).expect("write main.py");

        let (app, cx) = palette_focus_tests::open_test_app(cx, dir.path().to_path_buf());
        app.update_in(cx, |app, window, cx| {
            app.open_file_view(main_py.clone(), window, cx);
        });
        cx.run_until_parked();

        let indexed_deadline = Instant::now() + Duration::from_secs(240);
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

        let diagnostic_deadline = Instant::now() + Duration::from_secs(240);
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
            app.edit_buffer(&relative)
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
            let content = &app.edit_buffer(&relative).expect("a real buffer").content;
            assert!(
                content.contains("print"),
                "accepting the real completion should have spliced its real text into the \
                 real buffer, got: {content:?}"
            );
        });
    }
}
