//! An LSP client: spawns a language server as a plain child process (`std::process::Command`
//! with `Stdio::piped()`), deliberately not `pty-core` - see this crate's top-level docs for
//! why a pty's line discipline would corrupt JSON-RPC framing. Drives an
//! `initialize`/`initialized` handshake, and exposes request/response correlation plus a
//! `textDocument/publishDiagnostics` notification sink.
//!
//! ## Handshake order - verified against the LSP spec and `vendor/zed/crates/lsp/src/lsp.rs`
//!
//! The LSP spec is unambiguous on two points this implementation follows exactly:
//!
//! 1. `initialize` must be the **first** request sent, and the client must wait for its
//!    response before sending anything else except a reply to a server-initiated request the
//!    spec explicitly allows mid-handshake (`window/showMessageRequest`) - not exercised here.
//! 2. The `initialized` notification must be sent **after** the `initialize` response arrives,
//!    and every other request/notification (`textDocument/didOpen` included) must wait until
//!    *after* `initialized` has been sent - `vendor/zed/crates/lsp/src/lsp.rs`'s own
//!    `LanguageServer::initialize` sends them in exactly this order before returning a ready
//!    server handle, matching [`LspClient::spawn`]'s shape below: `spawn` does not return a
//!    usable [`LspClient`] until both steps have completed, so the type system only ever hands
//!    out an already-initialized client.
//!
//! Getting this order wrong is silent, not a hard error: a server that receives requests
//! before `initialized` (or before `initialize`'s response) is permitted by the spec to just
//! ignore them - "nothing happens and there's no error to debug," exactly the failure mode
//! this ordering guarantee prevents.
//!
//! ## Why a plain `Mutex`-guarded writer, not a dedicated writer thread
//!
//! `pty-core::PtySession::write_input` hands bytes to a dedicated writer thread because its
//! callers can be the GPUI foreground thread (a key-handler), where blocking on a full pty
//! write buffer would freeze the UI. Every write this crate performs ([`LspClient::request`],
//! [`LspClient::notify`]) is only ever called from inside `cx.background_executor().spawn(..)`
//! by this workspace's convention, so blocking the *calling* background thread for the
//! duration of a `write_all` to a child's stdin pipe (a small, fast syscall in the common
//! case) is an acceptable, simpler alternative to a second thread and channel here.
//!
//! ## Why no self-pipe for reader-thread shutdown, unlike `pty-core`
//!
//! `pty-core`'s reader thread needs a self-pipe because a pty master fd can have multiple
//! independent `dup`'d references alive at once, so dropping any *one* of them doesn't
//! guarantee the reader's blocking read unblocks. A `std::process::Child`'s `ChildStdout` has
//! no such ambiguity: once the child process terminates, the kernel closes every fd it held
//! (including the stdout pipe's write end) as part of termination, not merely once it's been
//! `wait()`-ed on - so the reader thread's blocking `read` reliably returns `Ok(0)` shortly
//! after the child dies, with no extra signaling needed. See [`LspClient::shutdown`]/`Drop`'s
//! docs for how process termination is guaranteed before those code paths return.

use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use lsp_types::notification::Notification as LspNotification;
use lsp_types::request::Request as LspRequest;
use lsp_types::{
    ClientCapabilities, DidOpenTextDocumentParams, HoverClientCapabilities, InitializeParams,
    InitializedParams, MarkupKind, PublishDiagnosticsClientCapabilities,
    TextDocumentClientCapabilities, TextDocumentItem, Uri, WorkspaceClientCapabilities,
    WorkspaceFolder,
};

use crate::proc;
use crate::transport;

/// Answers a real `workspace/configuration` request's `section` (`None` for a scope-less,
/// whole-item request) with the value this server should be told for it - see
/// [`server_request_reply`]'s docs for why a bare `null` isn't always legal/safe, and
/// [`default_workspace_configuration`] for the shared default every server not named here uses.
/// A plain `fn` pointer (not a `Box<dyn Fn>`/closure) since every real value needed here is
/// known statically per language - see `crate::language`'s registry in the `app` crate for where
/// a per-server one gets built.
pub type WorkspaceConfigFn = fn(section: Option<&str>) -> serde_json::Value;

/// The shared default [`WorkspaceConfigFn`]: a real, spec-legal empty object `{}` for every
/// section, correct for a server whose behavior doesn't depend on real settings coming back
/// (rust-analyzer, typescript-language-server both tolerate this - see [`ServerSpawnConfig`]'s
/// docs). Spec-legal because `workspace/configuration`'s result type is "the requested
/// configuration item, or `null` if not found" - `{}` is a real, present, empty configuration
/// object, a different (and for these servers, safer - see this module's top-level docs on why
/// `null` can leave a server assuming stale/default settings) answer than "not found".
pub fn default_workspace_configuration(_section: Option<&str>) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Real, per-language-server spawn configuration - the generalization point this crate exists
/// to expose so it's no longer hardcoded to rust-analyzer (see this crate's top-level docs).
/// This struct only defines the shape; `crate::language` in the `app` crate is the single real
/// source of truth for which values fill it in for each supported language.
#[derive(Debug, Clone)]
pub struct ServerSpawnConfig {
    /// Human-readable server name used in every log/error message this crate produces (e.g.
    /// `"rust-analyzer"`, `"typescript-language-server"`, `"pyright"`) - see [`LspError`]'s docs
    /// for why every variant now carries this instead of a hardcoded `"rust-analyzer"` string.
    pub name: &'static str,
    /// The binary [`Command::new`] spawns, looked up on `$PATH`.
    pub binary: &'static str,
    /// Real command-line arguments (e.g. `["--stdio"]` for typescript-language-server/
    /// pyright-langserver/vue-language-server; rust-analyzer needs none). A `Vec<String>`
    /// (not a `&'static [&'static str]`) so a real spawn config can carry a runtime-computed
    /// value if a future language ever needs one.
    pub args: Vec<String>,
    /// The real `InitializeParams.initialization_options` payload for this server, if any -
    /// `None` for a server that behaves well with none (rust-analyzer,
    /// typescript-language-server), `Some` for one that expects real, server-specific settings
    /// up front (Pyright - see `crate::language`'s registry in the `app` crate for the actual
    /// value it builds).
    pub initialization_options: Option<serde_json::Value>,
    /// Answers this server's real `workspace/configuration` requests - see
    /// [`WorkspaceConfigFn`]/[`default_workspace_configuration`]'s docs.
    pub workspace_configuration: WorkspaceConfigFn,
}

/// How long [`LspClient::spawn`] waits for `rust-analyzer`'s `initialize` **response**
/// specifically. Per the LSP spec a server may (and rust-analyzer does) respond to
/// `initialize` promptly with its capabilities and only *begin* real indexing afterwards
/// (reported via `$/progress` and, eventually, `textDocument/publishDiagnostics`) - so this is
/// deliberately much shorter than how long real diagnostics themselves might take to arrive for
/// a large project, not a budget for "finish indexing".
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`LspClient::shutdown`] waits for a response to the real `shutdown` request before
/// giving up on a graceful reply and proceeding to terminate the process anyway.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long [`LspClient::shutdown`] waits, after signaling `SIGTERM`, for the real process (and
/// any real descendants) to exit voluntarily before escalating to `SIGKILL`.
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(800);
/// Bound on how many un-drained "diagnostics changed" wake signals [`LspClient::drain_updates`]'s
/// channel buffers - a slow poller just coalesces catch-up ticks into fewer wakeups (each one
/// re-checks real current state via [`LspClient::diagnostics_for`], so a dropped/coalesced wake
/// never loses a real diagnostic, only a redundant "something changed" nudge).
const WAKE_CHANNEL_CAPACITY: usize = 64;

/// Locks a `Mutex`, recovering from poisoning rather than propagating a panic across it - a
/// poisoned lock here (some *other* thread already panicked while holding it) shouldn't
/// cascade into every subsequent caller panicking too. The state it protects
/// (pending-request bookkeeping, the diagnostics map) has no invariant that a mid-operation
/// panic could leave corrupt in a way that would make continuing unsafe.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Everything that can go wrong spawning, driving, or tearing down an [`LspClient`]. Mirrors
/// `pty_core::PtyError`'s shape (a `thiserror` enum, no `anyhow::Error` - see that type's docs
/// for why: `anyhow::Error` doesn't implement `std::error::Error`, so it can't be a `#[source]`
/// field, and would leak an opaque dependency type into this crate's public API).
#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("failed to spawn `{server}` (is it installed and on PATH?): {source}")]
    Spawn {
        server: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("repository root {0:?} does not exist or is not a directory")]
    InvalidRoot(PathBuf),
    #[error("path {0:?} could not be converted to a file:// URI (it must be absolute)")]
    InvalidPath(PathBuf),
    #[error(
        "URI {0:?} could not be converted to a real filesystem path (not a `file://` URI, or \
         malformed)"
    )]
    InvalidUri(String),
    #[error("{server}'s child process did not expose a piped stdio handle")]
    MissingStdio { server: &'static str },
    #[error("I/O error communicating with {server}: {source}")]
    Io {
        server: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to serialize an LSP request/notification's params: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to deserialize {server}'s response: {source}")]
    Deserialize {
        server: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("no response to `{method}` from {server} within the timeout")]
    Timeout {
        server: &'static str,
        method: &'static str,
    },
    #[error("{server} closed the connection")]
    ConnectionClosed { server: &'static str },
    #[error("{server} returned an error response to `{method}`: {message} (code {code})")]
    Response {
        server: &'static str,
        method: &'static str,
        code: i64,
        message: String,
    },
}

type PendingResponse = Result<serde_json::Value, (i64, String)>;

/// The real outcome of one [`LspClient::wait_for_update`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientUpdate {
    /// A real "diagnostics changed" wake signal was received.
    Updated,
    /// No wake signal arrived before the real timeout elapsed.
    Timeout,
    /// The connection is gone - no further update will ever arrive.
    Closed,
}

/// A running `rust-analyzer` process for one repository root, already past an
/// `initialize`/`initialized` handshake (see this module's docs for why that ordering
/// guarantee is baked into [`LspClient::spawn`] rather than left to callers). Cloneable via
/// `Arc<LspClient>` at the call site (every method here takes `&self`, guarded internally by
/// `Mutex`es) - the `app` crate keeps one `Arc<LspClient>` per repository root, shared across
/// every open Rust file in that repo.
pub struct LspClient {
    /// The human-readable server name every log/error message this client produces is
    /// parameterized by - see [`ServerSpawnConfig::name`]'s docs.
    name: &'static str,
    child: Option<Child>,
    pid: u32,
    exited: bool,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
    /// Guarded by a `Mutex` (rather than a bare `Receiver<()>`) purely so `LspClient` itself is
    /// `Sync` - `std::sync::mpsc::Receiver` is `Send` but deliberately not `Sync`, and this
    /// crate's callers (the `app` crate) share one `Arc<LspClient>` across a GPUI background
    /// task and a poll loop, both of which require `Arc<LspClient>: Send` and thus
    /// `LspClient: Sync`. There is still only one logical consumer in practice (see
    /// [`LspClient::drain_updates`]'s docs), so the lock is uncontended in the common case.
    wake_rx: Mutex<Receiver<()>>,
    reader_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl LspClient {
    /// Spawns the server described by `config` for the repository rooted at `repo_root`,
    /// performs an `initialize` request (awaiting its response) followed by an `initialized`
    /// notification - in that order, per this module's docs - and returns a client that's ready
    /// for `didOpen`/other calls. `repo_root` must be an absolute, existing directory (relative
    /// paths cannot be turned into a well-formed `file://` URI - see [`path_to_uri`]).
    pub fn spawn(repo_root: &Path, config: ServerSpawnConfig) -> Result<Self, LspError> {
        if !repo_root.is_dir() {
            return Err(LspError::InvalidRoot(repo_root.to_path_buf()));
        }
        // Canonicalized so the `file://` URI sent as this workspace folder's root is the same
        // symlink-resolved path every other `path_to_uri` call (e.g. from `did_open`) will
        // independently arrive at for a file underneath it, rather than assuming the caller
        // already passed a canonical path.
        let repo_root = repo_root
            .canonicalize()
            .map_err(|_| LspError::InvalidRoot(repo_root.to_path_buf()))?;

        let name = config.name;
        let mut command = Command::new(config.binary);
        command
            .args(&config.args)
            .current_dir(&repo_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|source| LspError::Spawn {
            server: name,
            source,
        })?;
        let pid = child.id();

        let stdin = child
            .stdin
            .take()
            .ok_or(LspError::MissingStdio { server: name })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(LspError::MissingStdio { server: name })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(LspError::MissingStdio { server: name })?;

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(WAKE_CHANNEL_CAPACITY);

        let workspace_configuration = config.workspace_configuration;
        let reader_thread = std::thread::spawn({
            let pending = Arc::clone(&pending);
            let diagnostics = Arc::clone(&diagnostics);
            let stdin_for_replies = Arc::clone(&stdin);
            move || {
                run_reader_loop(
                    stdout,
                    pending,
                    diagnostics,
                    wake_tx,
                    stdin_for_replies,
                    workspace_configuration,
                )
            }
        });
        // A server's stderr is diagnostic/log output (not part of the LSP protocol) - drained
        // on its own thread so a full OS pipe buffer on stderr can never backpressure the
        // server's stdout writes. Logged at debug level rather than discarded, so a startup
        // failure (e.g. a version mismatch panic) is still observable.
        let stderr_thread = std::thread::spawn(move || run_stderr_drain_loop(stderr, name));

        let client = LspClient {
            name,
            child: Some(child),
            pid,
            exited: false,
            stdin,
            next_id: AtomicI64::new(1),
            pending,
            diagnostics,
            wake_rx: Mutex::new(wake_rx),
            reader_thread: Some(reader_thread),
            stderr_thread: Some(stderr_thread),
        };

        client.initialize(&repo_root, config.initialization_options)?;
        Ok(client)
    }

    /// The human-readable server name this client was spawned with (`config.name`, see
    /// [`ServerSpawnConfig::name`]'s docs) - exposed so a caller (`crate::root::lsp` in the
    /// `app` crate) can build its own server-specific log messages without re-deriving the name
    /// from the process it already holds a handle to.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The handshake body: see this module's top-level docs for why the request and
    /// notification are sent in exactly this order and why no other call can happen first.
    /// `initialization_options` is the real, server-specific value from
    /// [`ServerSpawnConfig::initialization_options`] (`None` for a server that needs none).
    fn initialize(
        &self,
        repo_root: &Path,
        initialization_options: Option<serde_json::Value>,
    ) -> Result<(), LspError> {
        let uri = path_to_uri(repo_root)?;
        let folder_name = repo_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| repo_root.to_string_lossy().into_owned());

        // Explicitly prefers `PlainText` hover content (per item 7's generalization): a server
        // that respects this (rust-analyzer already only ever sends `PlainText` by its own
        // default - see `crate::root::hover_view`'s docs in the `app` crate) sends parseable
        // plain text instead of Markdown; a server that ignores it anyway (observed for real
        // against typescript-language-server/pyright-langserver - see that same module's docs
        // for the real fallback this drives) still gets handled, just via a degrade-to-plain-text
        // pass on the caller's side rather than a crash or raw Markdown syntax on screen.
        let capabilities = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                hover: Some(HoverClientCapabilities {
                    dynamic_registration: None,
                    content_format: Some(vec![MarkupKind::PlainText]),
                }),
                // A real, live-verified interop gap surfaced by generalizing past
                // rust-analyzer (which pushes `publishDiagnostics` unconditionally regardless of
                // advertised capabilities): `typescript-language-server` was directly observed,
                // via a live probe while building this integration, to never send a single
                // `publishDiagnostics` notification - not even an empty one - for a real
                // `didOpen`'d file until this capability is explicitly advertised. Harmless to
                // set unconditionally for every server, including ones (rust-analyzer, Pyright)
                // that don't require it.
                publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                    related_information: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            // Real support for `workspace/configuration` now genuinely exists (see
            // `ServerSpawnConfig::workspace_configuration`/[`server_request_reply`]'s docs), so
            // this is advertised for real rather than left unset - Pyright in particular relies
            // on this to decide it's safe to ask for real settings instead of assuming defaults.
            workspace: Some(WorkspaceClientCapabilities {
                configuration: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        // `root_uri` is deprecated by the spec in favor of `workspace_folders` (below), and
        // rust-analyzer never needed it - but generalizing this client past rust-analyzer (this
        // crate's whole reason for existing as of Revision R8) surfaced a real, live-verified
        // interop gap: `typescript-language-server`'s own TypeScript-discovery walk (finding a
        // real, project-local `node_modules/typescript`) was directly observed, via a live probe
        // while building this generalization, to fail with "Could not find a valid TypeScript
        // installation" when only `workspace_folders` is sent - `root_uri` specifically is what
        // it consults, `workspace_folders` alone isn't enough for that server's own real
        // implementation despite being the modern, spec-preferred field. Sending both is legal
        // per spec and does not regress rust-analyzer (still passes every one of its own e2e
        // tests below with `root_uri` now set) - real, both-fields-populated behavior, not a
        // guess.
        #[allow(deprecated)]
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            initialization_options,
            capabilities,
            root_uri: Some(uri.clone()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: uri.clone(),
                name: folder_name,
            }]),
            ..Default::default()
        };

        let _result = self.request::<lsp_types::request::Initialize>(params, INITIALIZE_TIMEOUT)?;
        self.notify::<lsp_types::notification::Initialized>(InitializedParams {})?;
        Ok(())
    }

    /// Sends a `textDocument/didOpen` notification for `path` with `text` as its current
    /// content, tagged with `language_id` (e.g. `"rust"`, `"typescript"`, `"tsx"`, `"python"` -
    /// see `crate::language` in the `app` crate for the real per-extension mapping; even "the
    /// TypeScript server" needs a real language id that varies by extension, not one constant).
    /// Never called before `initialized` (see this module's docs) since a caller can only ever
    /// hold an already-initialized `LspClient`.
    pub fn did_open(
        &self,
        path: &Path,
        text: String,
        version: i32,
        language_id: &str,
    ) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id: language_id.to_string(),
                version,
                text,
            },
        };
        self.notify::<lsp_types::notification::DidOpenTextDocument>(params)
    }

    /// The most recent `textDocument/publishDiagnostics` payload rust-analyzer has sent for
    /// `path`, if any has arrived yet. `None` means "no publishDiagnostics notification for
    /// this file has been received yet" - distinct from `Some(vec![])` ("rust-analyzer has
    /// analyzed this file and found zero diagnostics") - see [`Self::has_diagnostics_result`]
    /// for the same distinction under a name that makes the "haven't heard back yet" case
    /// explicit at call sites.
    pub fn diagnostics_for(&self, path: &Path) -> Option<Vec<lsp_types::Diagnostic>> {
        let uri = path_to_uri(path).ok()?;
        self.diagnostics_for_uri(&uri)
    }

    /// `true` once at least one `publishDiagnostics` notification has been received for `path`
    /// (even if it carried zero diagnostics - a clean-file result) - the signal the `app` crate
    /// uses to distinguish "rust-analyzer is still indexing/hasn't analyzed this file yet" from
    /// "rust-analyzer analyzed it and found nothing to report".
    pub fn has_diagnostics_result(&self, path: &Path) -> bool {
        match path_to_uri(path) {
            Ok(uri) => self.has_diagnostics_result_uri(&uri),
            Err(_) => false,
        }
    }

    /// Computes the same `file://` [`Uri`] [`Self::diagnostics_for`]/
    /// [`Self::has_diagnostics_result`] each derive internally from a path - exposed so a
    /// caller that needs more than one diagnostic lookup for the *same* path in one pass (e.g.
    /// `crate::root::AdeApp::render_file_view`, which calls into this client up to three times
    /// per render for one open file) can compute the [`Uri`] once and reuse it via
    /// [`Self::diagnostics_for_uri`]/[`Self::has_diagnostics_result_uri`], rather than paying
    /// [`path_to_uri`]'s blocking `canonicalize()` syscall repeatedly for the same render pass -
    /// a measured per-repaint cost on `uniform_list`'s virtualized rows. An associated function
    /// (not a method) since it needs no `&self`.
    pub fn uri_for_path(path: &Path) -> Result<Uri, LspError> {
        path_to_uri(path)
    }

    /// The inverse of [`Self::uri_for_path`]: converts a `file://` [`Uri`] (as returned in a
    /// `textDocument/definition` response's `Location`/`LocationLink` - H3's go-to-definition
    /// flow, `crate::root::AdeApp::trigger_goto_definition` in the `app` crate) back into an
    /// absolute filesystem [`PathBuf`] so the File view can load and display it.
    ///
    /// `rust-analyzer` can (and does, for e.g. a virtual macro-expansion buffer or a library
    /// without downloaded sources) return a non-`file://` URI scheme; this honestly fails with
    /// [`LspError::InvalidUri`] rather than guessing at a path for one, which
    /// `crate::root::AdeApp::trigger_goto_definition` treats as "no real navigation possible for
    /// this result" rather than crashing or fabricating a path. An associated function (not a
    /// method), mirroring [`Self::uri_for_path`] - conversion between an LSP protocol shape and
    /// a filesystem path, not something that touches any live `LspClient` state.
    pub fn path_for_uri(uri: &Uri) -> Result<PathBuf, LspError> {
        uri_to_path(uri)
    }

    /// Diagnostics lookup keyed by an already-computed [`Uri`] (see [`Self::uri_for_path`]'s
    /// docs for why this exists) - identical semantics to [`Self::diagnostics_for`], just
    /// without re-deriving the `Uri` from a path internally.
    pub fn diagnostics_for_uri(&self, uri: &Uri) -> Option<Vec<lsp_types::Diagnostic>> {
        lock(&self.diagnostics).get(uri.as_str()).cloned()
    }

    /// "Has a result arrived yet" check keyed by an already-computed [`Uri`] - identical
    /// semantics to [`Self::has_diagnostics_result`]; see [`Self::uri_for_path`]'s docs.
    pub fn has_diagnostics_result_uri(&self, uri: &Uri) -> bool {
        lock(&self.diagnostics).contains_key(uri.as_str())
    }

    /// Non-blocking: drains every "diagnostics changed" wake signal currently buffered (the
    /// reader thread sends one every time it records a fresh `publishDiagnostics` notification
    /// for *any* file, not just one specific path), returning `true` iff at least one was
    /// found. A caller polling this (see `crate::root`'s `cx.background_executor().timer(..)`
    /// poll pattern) knows to re-check [`Self::diagnostics_for`]/[`Self::has_diagnostics_result`]
    /// for whichever file it cares about and re-render if the answer changed.
    pub fn drain_updates(&self) -> bool {
        let receiver = lock(&self.wake_rx);
        let mut any = false;
        while receiver.try_recv().is_ok() {
            any = true;
        }
        any
    }

    /// Blocking, bounded wait for the next wake signal - see [`Self::drain_updates`]'s docs for
    /// what it means. Exists for deterministic test/tooling waits; `crate::root`'s actual GPUI
    /// polling always uses the non-blocking [`Self::drain_updates`] instead, since blocking is
    /// never acceptable on a GPUI-managed task.
    pub fn wait_for_update(&self, timeout: Duration) -> ClientUpdate {
        let receiver = lock(&self.wake_rx);
        match receiver.recv_timeout(timeout) {
            Ok(()) => ClientUpdate::Updated,
            Err(mpsc::RecvTimeoutError::Timeout) => ClientUpdate::Timeout,
            Err(mpsc::RecvTimeoutError::Disconnected) => ClientUpdate::Closed,
        }
    }

    /// Sends a framed LSP request and blocks the calling thread (see this module's docs on why
    /// that's acceptable here) for a response, up to `timeout`.
    pub fn request<R: LspRequest>(
        &self,
        params: R::Params,
        timeout: Duration,
    ) -> Result<R::Result, LspError> {
        let params_value = serde_json::to_value(params).map_err(LspError::Serialize)?;
        let result_value = self.send_request_raw(R::METHOD, params_value, timeout)?;
        serde_json::from_value(result_value).map_err(|source| LspError::Deserialize {
            server: self.name,
            source,
        })
    }

    /// Sends a framed LSP notification (no response expected or awaited).
    pub fn notify<N: LspNotification>(&self, params: N::Params) -> Result<(), LspError> {
        let params_value = serde_json::to_value(params).map_err(LspError::Serialize)?;
        self.send_notification_raw(N::METHOD, params_value)
    }

    fn send_request_raw(
        &self,
        method: &'static str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::sync_channel::<PendingResponse>(1);
        lock(&self.pending).insert(id, tx);

        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let mut stdin = lock(&self.stdin);
            if let Err(source) = transport::write_message(&mut *stdin, &message) {
                lock(&self.pending).remove(&id);
                return Err(LspError::Io {
                    server: self.name,
                    source,
                });
            }
        }

        match rx.recv_timeout(timeout) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err((code, message))) => Err(LspError::Response {
                server: self.name,
                method,
                code,
                message,
            }),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                lock(&self.pending).remove(&id);
                Err(LspError::Timeout {
                    server: self.name,
                    method,
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(LspError::ConnectionClosed { server: self.name })
            }
        }
    }

    fn send_notification_raw(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<(), LspError> {
        let message = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut stdin = lock(&self.stdin);
        transport::write_message(&mut *stdin, &message).map_err(|source| LspError::Io {
            server: self.name,
            source,
        })
    }

    /// Deterministically tears the session down: a best-effort `shutdown` request
    /// (rust-analyzer may already be unresponsive, which is not itself an error for teardown
    /// purposes), an `exit` notification, then `SIGTERM` to the process (and any descendants it
    /// spawned, see `crate::proc`'s docs), a bounded grace period, `SIGKILL` if still alive, a
    /// blocking reap, and finally joining the reader/stderr threads (which exit on their own
    /// once the process is confirmed dead, see this module's top-level docs on why no explicit
    /// shutdown signal is needed for them, unlike `pty-core`'s pty case). Safe to call more
    /// than once.
    pub fn shutdown(&mut self) -> Result<(), LspError> {
        if !self.exited {
            let _ = self.request::<lsp_types::request::Shutdown>((), SHUTDOWN_REQUEST_TIMEOUT);
            let _ = self.notify::<lsp_types::notification::Exit>(());

            proc::terminate_tree(self.pid, SHUTDOWN_GRACE_PERIOD);
            if let Some(child) = self.child.as_mut() {
                let _ = child.wait();
            }
            self.exited = true;
        }

        if let Some(handle) = self.reader_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if !self.exited {
            // `Drop` must not block the caller for long (the same discipline
            // `pty_core::PtySession::drop`'s own docs establish) - no graceful `shutdown`
            // request/grace period here, straight to `SIGKILL` for the whole real process tree.
            let descendants = proc::collect_descendant_pids(self.pid);
            proc::signal_pid(self.pid, nix::sys::signal::Signal::SIGKILL);
            for pid in &descendants {
                proc::signal_pid(*pid, nix::sys::signal::Signal::SIGKILL);
            }

            let reaped_immediately = self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            if reaped_immediately.is_none() {
                if let Some(mut child) = self.child.take() {
                    // `SIGKILL` was just sent; the process dies essentially immediately, but
                    // `try_wait` may have run a moment too early to observe it. Hand the handle
                    // to a short-lived detached thread that finishes `wait()`-ing so it gets
                    // reaped instead of lingering as a zombie, without making `drop` itself
                    // block - mirrors `pty_core::PtySession::drop`'s own exact reasoning.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
        }
        // The reader/stderr `JoinHandle`s are intentionally not joined here - the process is
        // dead or dying, so both threads will observe EOF and exit on their own shortly. Their
        // handles simply drop as the rest of `self` goes out of scope, which detaches (does not
        // block on, and does not kill) the underlying OS threads.
    }
}

/// Converts an absolute filesystem path to a percent-encoded `file://` URI via the `url`
/// crate (`Url::from_file_path`) - deliberately not hand-rolled, since correct
/// percent-encoding of arbitrary path bytes (spaces, non-ASCII, ...) is exactly the kind of
/// "looks right for the happy path, silently wrong on real-world paths" trap not worth
/// reimplementing.
fn path_to_uri(path: &Path) -> Result<Uri, LspError> {
    // Best-effort canonicalization for consistency with `LspClient::spawn`'s own root-URI
    // canonicalization; falls back to the given path as-is if canonicalization fails (e.g. a
    // caller checking a path that doesn't exist on disk).
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let url = url::Url::from_file_path(&canonical)
        .map_err(|_| LspError::InvalidPath(path.to_path_buf()))?;
    url.as_str()
        .parse::<Uri>()
        .map_err(|_| LspError::InvalidPath(path.to_path_buf()))
}

/// The inverse of [`path_to_uri`] - see [`LspClient::path_for_uri`]'s docs for why this
/// exists and how a non-`file://` result is handled by its caller. Parses `uri`'s string
/// form with the `url` crate (the same dependency [`path_to_uri`] uses for the opposite
/// direction) rather than hand-rolling percent-decoding.
fn uri_to_path(uri: &Uri) -> Result<PathBuf, LspError> {
    let url = url::Url::parse(uri.as_str())
        .map_err(|_| LspError::InvalidUri(uri.as_str().to_string()))?;
    if url.scheme() != "file" {
        return Err(LspError::InvalidUri(uri.as_str().to_string()));
    }
    url.to_file_path()
        .map_err(|_| LspError::InvalidUri(uri.as_str().to_string()))
}

/// Body of the background reader thread: reads framed messages from the server's stdout in
/// a loop, dispatching each one as a response (has `id`, no `method`), a server-initiated
/// request (has both `id` and `method` - auto-replied to; see the inline comment below for how),
/// or a notification (`method`, no `id`) - exits cleanly on EOF (the process died) or an I/O
/// error. `workspace_configuration` answers real `workspace/configuration` requests - see
/// [`server_request_reply`]'s docs.
fn run_reader_loop(
    stdout: std::process::ChildStdout,
    pending: Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
    wake_tx: SyncSender<()>,
    stdin: Arc<Mutex<ChildStdin>>,
    workspace_configuration: WorkspaceConfigFn,
) {
    let mut reader = BufReader::new(stdout);
    while let Ok(Some(value)) = transport::read_message(&mut reader) {
        handle_incoming(
            value,
            &pending,
            &diagnostics,
            &wake_tx,
            &stdin,
            workspace_configuration,
        );
    }
    // The connection is gone: drop every still-pending response sender so any thread blocked in
    // `recv_timeout` gets a real, immediate `Disconnected` rather than waiting out its own
    // timeout for a response that will now never arrive.
    lock(&pending).clear();
}

fn handle_incoming(
    value: serde_json::Value,
    pending: &Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>>,
    diagnostics: &Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
    wake_tx: &SyncSender<()>,
    stdin: &Arc<Mutex<ChildStdin>>,
    workspace_configuration: WorkspaceConfigFn,
) {
    let Some(object) = value.as_object() else {
        return;
    };

    if let Some(id) = object.get("id") {
        if object.contains_key("method") {
            // A server-initiated request (e.g. `workspace/configuration`,
            // `client/registerCapability`, `window/workDoneProgress/create`) - this phase's
            // scope is diagnostics only, so every such request is answered generically with a
            // `null` result rather than left unanswered, except `workspace/configuration` -
            // see [`server_request_reply`]'s docs for why that one gets a real, server-aware
            // reply instead.
            let method = object.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let reply =
                server_request_reply(id, method, object.get("params"), workspace_configuration);

            // Written from a short-lived, detached thread rather than inline from this reader
            // thread: `transport::write_message` is a blocking `write_all` to the child's
            // stdin pipe, and if that pipe's OS write buffer is full at this moment (e.g.
            // another thread is mid-write of a large un-chunked `textDocument/didOpen`),
            // writing here would block this reader thread from draining the child's stdout -
            // if the child is itself blocked writing to its own undrained stdout waiting for
            // more stdin, neither side can make progress. Server-initiated requests needing a
            // reply are rare (a handful per session, not a hot path), so the per-call
            // thread-spawn cost is negligible; the spawned thread reuses the same
            // lock-then-`write_message` path every other outbound message goes through
            // (`LspClient::send_request_raw`/`send_notification_raw`).
            let stdin = Arc::clone(stdin);
            std::thread::spawn(move || {
                let mut guard = lock(&stdin);
                let _ = transport::write_message(&mut *guard, &reply);
            });
            return;
        }

        // A response to one of our own requests.
        let Some(id) = id.as_i64() else { return };
        let sender = lock(pending).remove(&id);
        let Some(sender) = sender else { return };
        if let Some(error) = object.get("error") {
            let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("(no message)")
                .to_string();
            let _ = sender.send(Err((code, message)));
        } else {
            let result = object
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let _ = sender.send(Ok(result));
        }
        return;
    }

    // A notification - the only one this crate cares about is `publishDiagnostics`;
    // everything else (`$/progress`, `window/logMessage`, ...) is deliberately ignored here
    // rather than half-handled.
    let Some(method) = object.get("method").and_then(|m| m.as_str()) else {
        return;
    };
    if method == lsp_types::notification::PublishDiagnostics::METHOD {
        let Some(params) = object.get("params") else {
            return;
        };
        let Ok(parsed) =
            serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params.clone())
        else {
            return;
        };
        lock(diagnostics).insert(parsed.uri.as_str().to_string(), parsed.diagnostics);
        let _ = wake_tx.try_send(());
    }
}

/// Builds a protocol-shaped reply to one server-initiated request (`id` + `method` both
/// present on the incoming message - see [`handle_incoming`]'s docs for the reader-thread-side
/// handling this feeds). `workspace/configuration`
/// (`lsp_types::request::WorkspaceConfiguration`) is special-cased: its spec'd `Result` type
/// is `Vec<serde_json::Value>`, one entry per requested `ConfigurationItem`, so a bare
/// top-level `null` is not a legal reply shape for it, even though rust-analyzer tolerates one
/// in practice. Each item's own value now comes from `workspace_configuration` (the server's
/// real [`ServerSpawnConfig::workspace_configuration`]) rather than a hardcoded `null` - see
/// [`default_workspace_configuration`]'s docs for the shared default, and `crate::language`'s
/// registry in the `app` crate for Pyright's real, non-default one (this generalizes what used
/// to be a rust-analyzer-only special case: rust-analyzer sends this request even though this
/// client's capabilities never used to advertise `workspace.configuration`, and Pyright/
/// typescript-language-server were observed, while building this generalization, to send it
/// too). Every other server-initiated request method keeps the generic `null`-result fallback,
/// which remains legal for methods whose result types vary/are optional.
fn server_request_reply(
    id: &serde_json::Value,
    method: &str,
    params: Option<&serde_json::Value>,
    workspace_configuration: WorkspaceConfigFn,
) -> serde_json::Value {
    if method == lsp_types::request::WorkspaceConfiguration::METHOD {
        let items = params
            .and_then(|params| params.get("items"))
            .and_then(|items| items.as_array());
        let result: Vec<serde_json::Value> = items
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let section = item.get("section").and_then(|s| s.as_str());
                        workspace_configuration(section)
                    })
                    .collect()
            })
            .unwrap_or_default();
        return serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
    }

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": serde_json::Value::Null,
    })
}

/// Body of the stderr-draining background thread - see [`LspClient::spawn`]'s docs for why
/// this exists (preventing a full stderr pipe from backpressuring the process). Each line is
/// logged at `debug` level with a real `{server}:` prefix (not a hardcoded `"rust-analyzer:"`
/// one - see [`ServerSpawnConfig::name`]'s docs) rather than discarded, so a startup failure is
/// still observable in this app's own logs.
fn run_stderr_drain_loop(stderr: std::process::ChildStderr, server: &'static str) {
    use std::io::BufRead;
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        match line {
            Ok(line) => log::debug!("{server}: {line}"),
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Writes a minimal, valid cargo project to a fresh tempdir: a `Cargo.toml` and a
    /// `src/main.rs`. No external crates.io dependencies, so `cargo metadata`/rust-analyzer's
    /// workspace discovery never needs network access and indexes fast.
    fn write_scratch_project(main_rs: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"lsp_core_e2e_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write Cargo.toml");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir src");
        std::fs::write(dir.path().join("src").join("main.rs"), main_rs).expect("write main.rs");
        dir
    }

    /// The real `ServerSpawnConfig` for rust-analyzer this test module spawns against - the
    /// same shape `crate::language`'s registry builds in the `app` crate, kept as a local copy
    /// here (rather than a cross-crate dependency) since `lsp-core` must stand alone.
    fn rust_analyzer_config() -> ServerSpawnConfig {
        ServerSpawnConfig {
            name: "rust-analyzer",
            binary: "rust-analyzer",
            args: Vec::new(),
            initialization_options: None,
            workspace_configuration: default_workspace_configuration,
        }
    }

    /// Whether a `GotoDefinitionResponse` carries zero locations - a distinct "not resolved
    /// yet" case for the `Array`/`Link` shapes (an empty `Vec` is legal for either, spec-wise),
    /// used by [`rust_analyzer_returns_a_real_definition_location_for_a_call_site`] to keep
    /// polling rather than mistake it for a genuine zero-results answer. `Scalar` has no empty
    /// state (a single, always-present `Location`).
    fn goto_definition_response_is_empty(response: &lsp_types::GotoDefinitionResponse) -> bool {
        match response {
            lsp_types::GotoDefinitionResponse::Scalar(_) => false,
            lsp_types::GotoDefinitionResponse::Array(locations) => locations.is_empty(),
            lsp_types::GotoDefinitionResponse::Link(links) => links.is_empty(),
        }
    }

    /// Direct `/proc/<pid>` existence check, reused by every lifecycle test below - the same
    /// technique `pty-core`'s own tests use to prove a process is genuinely gone.
    fn pid_exists(pid: u32) -> bool {
        proc::pid_exists(pid)
    }

    #[test]
    fn workspace_configuration_gets_a_real_spec_shaped_array_reply_not_a_bare_null() {
        let id = serde_json::json!(7);
        let params = serde_json::json!({
            "items": [{ "section": "rust-analyzer" }, { "section": "rust" }]
        });

        let reply = server_request_reply(
            &id,
            lsp_types::request::WorkspaceConfiguration::METHOD,
            Some(&params),
            default_workspace_configuration,
        );

        assert_eq!(reply["id"], id);
        let result = reply
            .get("result")
            .expect("a result field should be present");
        let array = result
            .as_array()
            .expect("workspace/configuration's real result type is an array, not a bare null");
        assert_eq!(
            array.len(),
            2,
            "one array entry per requested ConfigurationItem"
        );
        // The default fn answers every section with a real, spec-legal empty object - not a
        // fabricated `null` and not a real per-section value (no per-section value is owed here
        // since the default is deliberately section-agnostic - see its own docs).
        assert!(array
            .iter()
            .all(|entry| entry.is_object() && !entry.is_null()));
    }

    #[test]
    fn workspace_configuration_with_no_items_gets_a_real_empty_array_not_null() {
        let id = serde_json::json!(1);
        let params = serde_json::json!({ "items": [] });

        let reply = server_request_reply(
            &id,
            lsp_types::request::WorkspaceConfiguration::METHOD,
            Some(&params),
            default_workspace_configuration,
        );

        let array = reply["result"]
            .as_array()
            .expect("still a real array, just empty");
        assert!(array.is_empty());
    }

    /// A server-aware `workspace_configuration` fn (the real shape `crate::language`'s Pyright
    /// entry in the `app` crate uses) is threaded all the way through to each array entry, keyed
    /// by that item's own real `section` - not the same value repeated for every item.
    #[test]
    fn workspace_configuration_uses_the_real_per_section_answer_from_the_server_fn() {
        fn fake_config(section: Option<&str>) -> serde_json::Value {
            match section {
                Some("python") => serde_json::json!({"pythonPath": "python3"}),
                _ => serde_json::Value::Object(serde_json::Map::new()),
            }
        }
        let id = serde_json::json!(9);
        let params = serde_json::json!({
            "items": [{ "section": "python" }, { "section": "python.analysis" }]
        });

        let reply = server_request_reply(
            &id,
            lsp_types::request::WorkspaceConfiguration::METHOD,
            Some(&params),
            fake_config,
        );

        let array = reply["result"].as_array().expect("real array");
        assert_eq!(array[0]["pythonPath"], "python3");
        assert_eq!(array[1], serde_json::json!({}));
    }

    #[test]
    fn every_other_server_initiated_request_keeps_the_generic_null_reply() {
        let id = serde_json::json!(3);
        let reply = server_request_reply(
            &id,
            "client/registerCapability",
            None,
            default_workspace_configuration,
        );
        assert_eq!(reply["id"], id);
        assert!(
            reply["result"].is_null(),
            "a method with no real special case should keep the legal generic null reply"
        );
    }

    #[test]
    fn spawn_performs_a_real_handshake_and_shutdown_leaves_no_orphan() {
        let project = write_scratch_project("fn main() {}\n");
        let mut client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");
        let pid = client.pid;
        assert!(
            pid_exists(pid),
            "rust-analyzer's real pid {pid} should be alive right after a successful spawn"
        );
        // Captured *before* `shutdown()` - `proc::collect_descendant_pids`'s docs require this
        // ordering, since reading it after teardown starts races the kernel reparenting
        // children out from under `/proc/<pid>/task/<pid>/children`. Honest caveat: this
        // dependency-free scratch fixture may not cause rust-analyzer to spawn any descendant
        // within this test's short runtime, in which case the assertion below trivially passes
        // over an empty list - kept anyway since it costs nothing and exercises the same code
        // path a dependency-heavy project would.
        let descendants_before_shutdown = proc::collect_descendant_pids(pid);

        client.shutdown().expect("shutdown should succeed");

        for descendant_pid in &descendants_before_shutdown {
            assert!(
                !pid_exists(*descendant_pid),
                "descendant pid {descendant_pid} (of rust-analyzer pid {pid}) should be fully \
                 reaped after shutdown() returns, not left running"
            );
        }
        assert!(
            !pid_exists(pid),
            "rust-analyzer's real pid {pid} should be fully reaped (not even a zombie) \
             immediately after shutdown() returns"
        );
    }

    #[test]
    fn drop_without_shutdown_does_not_leave_an_orphaned_process() {
        let project = write_scratch_project("fn main() {}\n");
        let client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");
        let pid = client.pid;
        assert!(
            pid_exists(pid),
            "rust-analyzer's real pid {pid} should be alive after spawn"
        );

        drop(client);

        let deadline = Instant::now() + Duration::from_secs(10);
        while pid_exists(pid) {
            assert!(
                Instant::now() < deadline,
                "rust-analyzer's real pid {pid} was still alive 10s after LspClient was \
                 dropped with no explicit shutdown() call - orphaned process"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_second_request_after_the_connection_closes_fails_fast_not_after_the_full_timeout() {
        let project = write_scratch_project("fn main() {}\n");
        let mut client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");
        client.shutdown().expect("shutdown should succeed");

        let start = Instant::now();
        let result = client.request::<lsp_types::request::Shutdown>((), Duration::from_secs(30));
        assert!(
            matches!(
                result,
                Err(LspError::Io { .. }) | Err(LspError::ConnectionClosed { .. })
            ),
            "a request sent after the connection is torn down should fail with a real \
             connection-closed/IO error, got: {result:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "a request against a closed connection should fail fast, not wait out its own \
             30s timeout - took {:?}",
            start.elapsed()
        );
    }

    /// The real, practical end-to-end proof this phase exists to deliver: a real
    /// `rust-analyzer` process, spawned against a real (tiny, dependency-free) scratch cargo
    /// project containing a genuine type error, performs a real `initialize`/`initialized`
    /// handshake, receives a real `textDocument/didOpen`, and - asynchronously, on its own
    /// timeline - pushes back a real `textDocument/publishDiagnostics` notification that
    /// actually references the introduced mismatch. This is a genuinely slow test (real process
    /// startup plus real sysroot/std indexing, even for a trivial crate) - no artificial sleep
    /// stands in for that real wait, and no diagnostic is fabricated if the wait were to time
    /// out (the assertion below would simply fail honestly).
    ///
    /// Observed real behavior worth documenting (found while writing this test, via a temporary
    /// debug harness that logged every real `publishDiagnostics` payload with its arrival time):
    /// rust-analyzer publishes **twice** for a freshly-opened file - an initial, near-instant
    /// (~0.6s here) publish with an *empty* diagnostics array (its syntax-only pass, before
    /// semantic type-checking has run), immediately followed (~0.1s later, for this tiny fixture)
    /// by a second publish carrying the real `E0308` mismatch. This is real, correct, eventually-
    /// consistent LSP behavior (the same thing VS Code/any other real LSP client observes) - not
    /// a bug in this crate - so the wait loop below deliberately keeps waiting for a real
    /// *non-empty* result (which this fixture is deterministically known to eventually produce),
    /// rather than stopping at the first `has_diagnostics_result` flip - see that method's own
    /// docs, and the step report's "indexing state" section, for why `has_diagnostics_result`
    /// itself is still the right, honest signal for the UI layer even though it can be `true`
    /// with a since-superseded empty result for a brief real window like this one.
    #[test]
    fn rust_analyzer_reports_a_real_diagnostic_for_a_real_type_error() {
        let project = write_scratch_project(
            "fn main() {\n    let x: i32 = \"not a number\";\n    println!(\"{}\", x);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");
        let source = std::fs::read_to_string(&main_rs).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");

        client
            .did_open(&main_rs, source, 1, "rust")
            .expect("didOpen should send successfully");

        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            let has_real_diagnostics = client
                .diagnostics_for(&main_rs)
                .is_some_and(|diagnostics| !diagnostics.is_empty());
            if has_real_diagnostics {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "rust-analyzer never published a non-empty diagnostics set for the fixture \
                 file within 180s"
            );
            match client.wait_for_update(remaining.min(Duration::from_secs(5))) {
                ClientUpdate::Updated | ClientUpdate::Timeout => continue,
                ClientUpdate::Closed => {
                    panic!("rust-analyzer's connection closed before publishing any diagnostics")
                }
            }
        }

        let diagnostics = client.diagnostics_for(&main_rs).expect(
            "a diagnostics result should be present - the loop above only exits once one is",
        );
        assert!(
            !diagnostics.is_empty(),
            "expected at least one real diagnostic for a genuine `let x: i32 = \"...\";` type \
             mismatch, got zero"
        );

        let mismatch = diagnostics.iter().find(|diagnostic| {
            let message = diagnostic.message.to_lowercase();
            message.contains("mismatched")
                || message.contains("expected") && message.contains("i32")
        });
        assert!(
            mismatch.is_some(),
            "expected a diagnostic referencing the real type mismatch, got: {diagnostics:#?}"
        );
        let mismatch = mismatch.expect("checked above");
        assert_eq!(
            mismatch.severity,
            Some(lsp_types::DiagnosticSeverity::ERROR),
            "a genuine type mismatch should be reported at ERROR severity, got: {mismatch:#?}"
        );
        // The diagnostic's own range should land on the real offending line (`let x: i32 = ...`
        // is line index 1, zero-based) - not just "some diagnostic came back from the process".
        assert_eq!(
            mismatch.range.start.line, 1,
            "expected the mismatch diagnostic's range to point at the real offending line, \
             got: {mismatch:#?}"
        );

        client.shutdown().expect("shutdown should succeed");
    }

    /// The real `ServerSpawnConfig` for typescript-language-server this test module spawns
    /// against - a self-contained test-local copy (not a cross-crate dependency on the `app`
    /// crate's `crate::language` registry, mirroring [`rust_analyzer_config`]'s own reasoning).
    fn typescript_language_server_config() -> ServerSpawnConfig {
        ServerSpawnConfig {
            name: "typescript-language-server",
            binary: "typescript-language-server",
            args: vec!["--stdio".to_string()],
            initialization_options: None,
            workspace_configuration: default_workspace_configuration,
        }
    }

    /// Writes a minimal real TypeScript project (`tsconfig.json` plus a `.ts` file) to a fresh
    /// tempdir, then does a real, live `npm install typescript@5` into it.
    ///
    /// This step is not optional/conservative - it was discovered live, while building this
    /// integration, to be genuinely required in this exact sandbox: `typescript-language-server`
    /// has no bundled TypeScript of its own and refuses to `initialize` at all ("Could not find a
    /// valid TypeScript installation") without a real, discoverable one, and this sandbox's own
    /// *global* `typescript` install happens to be pinned to the new native Go-based rewrite
    /// (`7.x`, `tsc`-only, no classic `lib/tsserver.js`), which does not satisfy that requirement
    /// either - confirmed by a live probe against it before writing this helper. A real,
    /// project-local classic `typescript@5` install is the one thing that reliably works.
    fn write_scratch_ts_project(main_ts: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(
            dir.path().join("tsconfig.json"),
            "{\"compilerOptions\": {\"strict\": true, \"target\": \"ES2020\"}}\n",
        )
        .expect("write tsconfig.json");
        std::fs::write(dir.path().join("main.ts"), main_ts).expect("write main.ts");
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
        assert!(
            status.success(),
            "npm install typescript@5 into the scratch project failed"
        );
        dir
    }

    /// The real, practical end-to-end proof for TypeScript (mirrors
    /// [`rust_analyzer_reports_a_real_diagnostic_for_a_real_type_error`] above exactly): a real
    /// `typescript-language-server`, spawned via the same generalized [`LspClient::spawn`] every
    /// other language now shares, against a real scratch project with a genuine
    /// `const bad: number = "not a number";` type mismatch, performs a real handshake, receives
    /// a real `didOpen` tagged with the real `"typescript"` language id, and asynchronously
    /// pushes back a real `textDocument/publishDiagnostics` referencing the introduced mismatch.
    #[test]
    fn typescript_language_server_reports_a_real_diagnostic_for_a_real_type_error() {
        let project =
            write_scratch_ts_project("const bad: number = \"not a number\";\nconsole.log(bad);\n");
        let main_ts = project.path().join("main.ts");
        let source = std::fs::read_to_string(&main_ts).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), typescript_language_server_config())
            .expect("spawning + initializing typescript-language-server should succeed");
        client
            .did_open(&main_ts, source, 1, "typescript")
            .expect("didOpen should send successfully");

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let has_real_diagnostics = client
                .diagnostics_for(&main_ts)
                .is_some_and(|diagnostics| !diagnostics.is_empty());
            if has_real_diagnostics {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "typescript-language-server never published a non-empty diagnostics set for \
                 the fixture file within 120s"
            );
            match client.wait_for_update(remaining.min(Duration::from_secs(5))) {
                ClientUpdate::Updated | ClientUpdate::Timeout => continue,
                ClientUpdate::Closed => {
                    panic!(
                        "typescript-language-server's connection closed before publishing any \
                         diagnostics"
                    )
                }
            }
        }

        let diagnostics = client.diagnostics_for(&main_ts).expect(
            "a diagnostics result should be present - the loop above only exits once one is",
        );
        let mismatch = diagnostics
            .iter()
            .find(|diagnostic| diagnostic.message.to_lowercase().contains("not assignable"));
        assert!(
            mismatch.is_some(),
            "expected a diagnostic referencing the real type mismatch, got: {diagnostics:#?}"
        );

        client.shutdown().expect("shutdown should succeed");
    }

    /// A real end-to-end proof of hover for TypeScript, mirroring
    /// [`rust_analyzer_returns_a_real_hover_for_a_documented_function`] above.
    #[test]
    fn typescript_language_server_returns_a_real_hover_for_a_documented_function() {
        let project = write_scratch_ts_project(
            "/**\n * Adds one to the given number.\n */\nfunction addOne(x: number): number \
             {\n  return x + 1;\n}\nconst result = addOne(41);\n",
        );
        let main_ts = project.path().join("main.ts");
        let source = std::fs::read_to_string(&main_ts).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), typescript_language_server_config())
            .expect("spawning + initializing typescript-language-server should succeed");
        client
            .did_open(&main_ts, source, 1, "typescript")
            .expect("didOpen should send successfully");

        let uri = path_to_uri(&main_ts).expect("real file:// URI for the fixture file");
        // Line 6 (0-based) is `const result = addOne(41);`; `"const result = "` is 15 real
        // ASCII bytes, so character 17 lands inside the real `addOne` call-site identifier.
        let params = lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position {
                    line: 6,
                    character: 17,
                },
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };

        let deadline = Instant::now() + Duration::from_secs(120);
        let hover = loop {
            match client.request::<lsp_types::request::HoverRequest>(
                params.clone(),
                Duration::from_secs(10),
            ) {
                Ok(Some(hover)) => break hover,
                Ok(None) | Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "typescript-language-server never returned a real hover for the \
                         fixture's call site within 120s"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };

        let lsp_types::HoverContents::Markup(markup) = &hover.contents else {
            panic!("expected a real Markup hover response, got: {hover:#?}");
        };
        // Real, observed-while-building-this-integration behavior, pinned here rather than just
        // narrated: even though this client's `ClientCapabilities` now requests `PlainText` as
        // the preferred `content_format` (see `LspClient::initialize`'s docs),
        // typescript-language-server was directly observed to still send `Markdown` regardless -
        // exactly the real "not every server honors that preference" case
        // `crate::hover_view`'s Markdown-degrade fallback (in the `app` crate) exists for. The
        // real, observed shape (captured verbatim while building this test):
        // `"\n```typescript\nfunction addOne(x: number): number\n```\nAdds one to the given \
        // number."`
        assert_eq!(
            markup.kind,
            lsp_types::MarkupKind::Markdown,
            "if typescript-language-server ever starts honoring the PlainText preference this \
             would be real, welcome news - but `crate::hover_view`'s Markdown-degrade fallback \
             was verified against real Markdown output, so a silent switch to PlainText deserves \
             a human look before trusting the fallback is still exercised for real"
        );
        assert!(
            markup.value.contains("addOne"),
            "expected the real hover text to mention the real function name, got: {:?}",
            markup.value
        );
        assert!(
            markup.value.contains("number"),
            "expected the real hover text to mention the function's real `number` signature \
             type, got: {:?}",
            markup.value
        );

        client.shutdown().expect("shutdown should succeed");
    }

    /// The real `ServerSpawnConfig` for pyright-langserver this test module spawns against -
    /// mirrors `crate::language`'s Pyright entry in the `app` crate (a self-contained test-local
    /// copy, same reasoning as [`typescript_language_server_config`]).
    fn pyright_config() -> ServerSpawnConfig {
        fn workspace_configuration(section: Option<&str>) -> serde_json::Value {
            let analysis = serde_json::json!({
                "autoSearchPaths": true,
                "useLibraryCodeForTypes": true,
                "diagnosticMode": "openFilesOnly",
            });
            match section {
                Some("python") => serde_json::json!({ "analysis": analysis }),
                Some("python.analysis") => analysis,
                _ => serde_json::Value::Object(serde_json::Map::new()),
            }
        }
        ServerSpawnConfig {
            name: "pyright-langserver",
            binary: "pyright-langserver",
            args: vec!["--stdio".to_string()],
            initialization_options: Some(serde_json::json!({
                "python": {
                    "analysis": {
                        "autoSearchPaths": true,
                        "useLibraryCodeForTypes": true,
                        "diagnosticMode": "openFilesOnly",
                    }
                }
            })),
            workspace_configuration,
        }
    }

    fn write_scratch_py_project(main_py: &str) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        std::fs::write(dir.path().join("main.py"), main_py).expect("write main.py");
        dir
    }

    /// The real, practical end-to-end proof for Python: a real `pyright-langserver`, spawned
    /// with the real, non-`null` `initializationOptions`/`workspace/configuration` answers this
    /// generalization added specifically because Pyright (unlike rust-analyzer/
    /// typescript-language-server) needs them to behave well, against a real scratch file with a
    /// genuine `x: int = "not a number"` type error, performs a real handshake, receives a real
    /// `didOpen` tagged `"python"`, and asynchronously pushes back a real diagnostic.
    #[test]
    fn pyright_reports_a_real_diagnostic_for_a_real_type_error() {
        let project = write_scratch_py_project(
            "def add_one(x: int) -> int:\n    return x + 1\n\nbad: int = \"not a number\"\n",
        );
        let main_py = project.path().join("main.py");
        let source = std::fs::read_to_string(&main_py).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), pyright_config())
            .expect("spawning + initializing pyright-langserver should succeed");
        client
            .did_open(&main_py, source, 1, "python")
            .expect("didOpen should send successfully");

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let has_real_diagnostics = client
                .diagnostics_for(&main_py)
                .is_some_and(|diagnostics| !diagnostics.is_empty());
            if has_real_diagnostics {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "pyright-langserver never published a non-empty diagnostics set for the \
                 fixture file within 120s"
            );
            match client.wait_for_update(remaining.min(Duration::from_secs(5))) {
                ClientUpdate::Updated | ClientUpdate::Timeout => continue,
                ClientUpdate::Closed => {
                    panic!(
                        "pyright-langserver's connection closed before publishing any diagnostics"
                    )
                }
            }
        }

        let diagnostics = client.diagnostics_for(&main_py).expect(
            "a diagnostics result should be present - the loop above only exits once one is",
        );
        // Real, observed-while-building-this-test Pyright message shape: it names the literal's
        // own real inferred type (`Literal['not a number']`), not a bare `str` - so this checks
        // for the real, distinguishing "not assignable ... int" wording actually seen, not a
        // guessed-at "str" substring.
        let mismatch = diagnostics.iter().find(|diagnostic| {
            let message = diagnostic.message.to_lowercase();
            message.contains("not assignable") && message.contains("int")
        });
        assert!(
            mismatch.is_some(),
            "expected a diagnostic referencing the real type mismatch, got: {diagnostics:#?}"
        );

        client.shutdown().expect("shutdown should succeed");
    }

    /// A real end-to-end proof of hover for Python, mirroring the TypeScript/rust-analyzer
    /// hover tests above.
    #[test]
    fn pyright_returns_a_real_hover_for_a_documented_function() {
        let project = write_scratch_py_project(
            "def add_one(x: int) -> int:\n    \"\"\"Adds one to the given number.\"\"\"\n    \
             return x + 1\n\n\nresult = add_one(41)\n",
        );
        let main_py = project.path().join("main.py");
        let source = std::fs::read_to_string(&main_py).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), pyright_config())
            .expect("spawning + initializing pyright-langserver should succeed");
        client
            .did_open(&main_py, source, 1, "python")
            .expect("didOpen should send successfully");

        let uri = path_to_uri(&main_py).expect("real file:// URI for the fixture file");
        // Line 5 (0-based) is `result = add_one(41)`; `"result = "` is 9 real ASCII bytes, so
        // character 11 lands inside the real `add_one` call-site identifier.
        let params = lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position {
                    line: 5,
                    character: 11,
                },
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };

        let deadline = Instant::now() + Duration::from_secs(120);
        let hover = loop {
            match client.request::<lsp_types::request::HoverRequest>(
                params.clone(),
                Duration::from_secs(10),
            ) {
                Ok(Some(hover)) => break hover,
                Ok(None) | Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "pyright-langserver never returned a real hover for the fixture's call \
                         site within 120s"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };

        let lsp_types::HoverContents::Markup(markup) = &hover.contents else {
            panic!("expected a real Markup hover response, got: {hover:#?}");
        };
        // Real, observed-while-building-this-test shape: `"(function) def add_one(x: int) -> \
        // int\n\nAdds one to the given number."` - unlike typescript-language-server, Pyright
        // was directly observed honoring the `PlainText` preference this client requests.
        assert_eq!(
            markup.kind,
            lsp_types::MarkupKind::PlainText,
            "Pyright was observed honoring the PlainText content_format preference for real - a \
             switch to Markdown deserves a human look, same reasoning as the TypeScript hover \
             test's own pinned assertion"
        );
        assert!(
            markup.value.contains("add_one"),
            "expected the real hover text to mention the real function name, got: {:?}",
            markup.value
        );

        client.shutdown().expect("shutdown should succeed");
    }

    #[test]
    fn uri_to_path_round_trips_with_path_to_uri_for_a_real_temp_file() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("real_file.rs");
        std::fs::write(&path, "fn main() {}\n").expect("write");

        let uri = path_to_uri(&path).expect("a real, existing absolute path should convert");
        let round_tripped = uri_to_path(&uri).expect("a real file:// URI should convert back");

        assert_eq!(
            round_tripped,
            path.canonicalize().expect("canonicalize"),
            "converting a real path to a URI and back should yield the same real, canonical path"
        );
        // `LspClient::path_for_uri` is a thin public wrapper over the same real logic - confirm
        // it agrees, not just that the private free function does.
        assert_eq!(
            LspClient::path_for_uri(&uri).expect("path_for_uri"),
            round_tripped
        );
    }

    #[test]
    fn uri_to_path_rejects_a_real_non_file_scheme_uri() {
        let uri: Uri = "https://example.com/not/a/file"
            .parse()
            .expect("a real, well-formed https URI should parse");
        let result = uri_to_path(&uri);
        assert!(
            matches!(result, Err(LspError::InvalidUri(_))),
            "a real non-file:// URI should honestly fail to convert, not silently misinterpret \
             its path segment as a real filesystem path - got {result:?}"
        );
    }

    /// A real, second end-to-end proof against a genuinely running `rust-analyzer`: a real
    /// `textDocument/hover` request at a real, byte-accurate position (a documented function's
    /// own call site) returns the function's real signature and real doc-comment prose - not a
    /// placeholder. This is the exact real fixture/technique
    /// [`rust_analyzer_reports_a_real_diagnostic_for_a_real_type_error`] above already
    /// established for diagnostics, reused here for hover: a real, tiny, dependency-free scratch
    /// crate (so indexing is fast and needs no network), a real spawn/`didOpen`, and a bounded,
    /// generous real wait - `rust-analyzer` needs to finish enough of its own real indexing to
    /// answer a hover query, which (like diagnostics) is not instantaneous even for a trivial
    /// fixture, so this polls with real retries rather than a single immediate request.
    #[test]
    fn rust_analyzer_returns_a_real_hover_for_a_documented_function() {
        let project = write_scratch_project(
            "/// Adds one to the given number.\n\
             ///\n\
             /// Returns the incremented value.\n\
             fn add_one(x: i32) -> i32 {\n    x + 1\n}\n\n\
             fn main() {\n    let result = add_one(41);\n    println!(\"{}\", result);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");
        let source = std::fs::read_to_string(&main_rs).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");
        client
            .did_open(&main_rs, source, 1, "rust")
            .expect("didOpen should send successfully");

        let uri = path_to_uri(&main_rs).expect("real file:// URI for the fixture file");
        // Byte-accurate real position: line 8 (0-based - the blank line the fixture's own
        // literal inserts between the two `fn`s, at line 6, shifts everything below it down by
        // one from a naive line count) is `    let result = add_one(41);` - character 20 lands
        // inside the real `add_one` call-site identifier.
        let params = lsp_types::HoverParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position {
                    line: 8,
                    character: 20,
                },
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
        };

        // 180s, matching `rust_analyzer_reports_a_real_diagnostic_for_a_real_type_error`'s own
        // deadline above - real sysroot/std indexing time, not an arbitrary number (see that
        // test's own docs).
        let deadline = Instant::now() + Duration::from_secs(180);
        let hover = loop {
            match client.request::<lsp_types::request::HoverRequest>(
                params.clone(),
                Duration::from_secs(10),
            ) {
                Ok(Some(hover)) => break hover,
                Ok(None) | Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "rust-analyzer never returned a real hover for the fixture's call site \
                         within 180s"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };

        let lsp_types::HoverContents::Markup(markup) = &hover.contents else {
            panic!("expected a real Markup hover response, got: {hover:#?}");
        };
        assert!(
            markup.value.contains("add_one"),
            "expected the real hover text to mention the real function name, got: {:?}",
            markup.value
        );
        assert!(
            markup.value.contains("i32"),
            "expected the real hover text to mention the function's real `i32` signature type, \
             got: {:?}",
            markup.value
        );
        assert!(
            markup.value.contains("Adds one to the given number"),
            "expected the real hover text to include the function's real doc comment, got: {:?}",
            markup.value
        );

        client.shutdown().expect("shutdown should succeed");
    }

    /// A real end-to-end proof of go-to-definition: a real `textDocument/definition` request at
    /// the same real call-site position the hover test above uses returns a real
    /// `GotoDefinitionResponse` whose location genuinely points back at the function's own real
    /// definition line in the same file - not a placeholder location.
    #[test]
    fn rust_analyzer_returns_a_real_definition_location_for_a_call_site() {
        let project = write_scratch_project(
            "fn add_one(x: i32) -> i32 {\n    x + 1\n}\n\n\
             fn main() {\n    let result = add_one(41);\n    println!(\"{}\", result);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");
        let source = std::fs::read_to_string(&main_rs).expect("read fixture source");

        let mut client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");
        client
            .did_open(&main_rs, source, 1, "rust")
            .expect("didOpen should send successfully");

        let uri = path_to_uri(&main_rs).expect("real file:// URI for the fixture file");
        // Line 5 (0-based) is `    let result = add_one(41);`; character 20 is inside the real
        // `add_one` call-site identifier, same as the hover test above.
        let params = lsp_types::GotoDefinitionParams {
            text_document_position_params: lsp_types::TextDocumentPositionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position: lsp_types::Position {
                    line: 5,
                    character: 20,
                },
            },
            work_done_progress_params: lsp_types::WorkDoneProgressParams::default(),
            partial_result_params: lsp_types::PartialResultParams::default(),
        };

        // 180s - see the hover test above's identical deadline for why.
        let deadline = Instant::now() + Duration::from_secs(180);
        let response = loop {
            match client.request::<lsp_types::request::GotoDefinition>(
                params.clone(),
                Duration::from_secs(10),
            ) {
                // A real, `Some`-but-empty `Array`/`Link` is a real, distinct "rust-analyzer
                // hasn't resolved this yet" state (observed directly while writing this test),
                // not the same as a genuine "no definition exists" answer for this fixture (which
                // is known, by construction, to always have one) - keep polling exactly like a
                // real `None` rather than failing on it.
                Ok(Some(response)) if !goto_definition_response_is_empty(&response) => {
                    break response
                }
                Ok(_) | Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "rust-analyzer never returned a real definition for the fixture's call \
                         site within 180s"
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        };

        // `GotoDefinitionResponse` is a real, untagged three-way union (`Scalar`/`Array`/`Link` -
        // see `lsp_types`' own docs on the type) - rust-analyzer was observed (while writing this
        // test) to reply with `Array`, but this match covers all three real shapes rather than
        // assuming one.
        let (result_uri, range) = match &response {
            lsp_types::GotoDefinitionResponse::Scalar(location) => {
                (location.uri.clone(), location.range)
            }
            lsp_types::GotoDefinitionResponse::Array(locations) => {
                let location = locations
                    .first()
                    .expect("a real definition response should carry at least one real location");
                (location.uri.clone(), location.range)
            }
            lsp_types::GotoDefinitionResponse::Link(links) => {
                let link = links
                    .first()
                    .expect("a real definition response should carry at least one real location");
                (link.target_uri.clone(), link.target_selection_range)
            }
        };
        assert_eq!(
            result_uri.as_str(),
            uri.as_str(),
            "the real definition should point back into the same real fixture file"
        );
        // `fn add_one` starts at line 0 (0-based) in this fixture - the real definition's own
        // range should land there, not at the call site it was requested from.
        assert_eq!(
            range.start.line, 0,
            "expected the real definition location to point at the real `fn add_one` line, got: \
             {range:?}"
        );

        client.shutdown().expect("shutdown should succeed");
    }
}
