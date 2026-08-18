//! An LSP client: spawns a language server as a piped child process, drives the
//! `initialize`/`initialized` handshake, and correlates requests to responses.
//!
//! [`LspClient::spawn`] does not return until both handshake steps complete, so the type system
//! only ever hands out an initialized client. Order matters and fails silently otherwise: the
//! spec lets a server ignore anything sent before `initialized`, so nothing happens and there is
//! no error to debug.
//!
//! Writes go through a `Mutex` rather than a writer thread, unlike `pty-core`: callers are always
//! on a background executor, and [`transport::write_message_bounded`] owns its own bounded wait -
//! so a server that stops reading costs one thread a [`WRITE_TIMEOUT`] and ends the connection,
//! rather than parking forever while holding the mutex everyone queues on.
//!
//! The reader needs no self-pipe either. A pty master can have several `dup`'d references, but
//! terminating a child closes every fd it held, so the reader's `read` returns `Ok(0)` shortly
//! after it dies.

use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lsp_types::notification::Notification as LspNotification;
use lsp_types::request::Request as LspRequest;
use lsp_types::{
    ClientCapabilities, CompletionClientCapabilities, CompletionItemCapability,
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, HoverClientCapabilities,
    InitializeParams, InitializedParams, MarkupKind, PublishDiagnosticsClientCapabilities,
    ServerCapabilities, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
    TextDocumentItem, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
    VersionedTextDocumentIdentifier, WorkspaceClientCapabilities, WorkspaceFolder,
};

#[cfg(unix)]
use crate::proc;
use crate::transport;

/// Answers one `workspace/configuration` section; `None` means a scope-less whole-item request.
///
/// A `fn` pointer rather than a closure: every value is known statically per language.
pub type WorkspaceConfigFn = fn(section: Option<&str>) -> serde_json::Value;

/// An empty object for every section - spec-legal, and a different answer from `null`, which
/// means "not found" and can leave a server assuming stale defaults.
pub fn default_workspace_configuration(_section: Option<&str>) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

#[derive(Debug, Clone)]
pub struct ServerSpawnConfig {
    pub name: &'static str,
    pub binary: &'static str,
    pub args: Vec<String>,
    pub initialization_options: Option<serde_json::Value>,
    pub workspace_configuration: WorkspaceConfigFn,
    /// Notification methods the caller wants queued for
    /// [`LspClient::drain_custom_notifications`], beyond the `publishDiagnostics` handled here.
    ///
    /// A subscription list rather than queueing everything unrecognized: otherwise a busy server's
    /// `$/progress` traffic is cloned and queued for callers that never read it, and a queue
    /// nobody drains sits permanently at its cap warning about itself.
    pub custom_notification_methods: Vec<&'static str>,
}

/// How long [`LspClient::spawn`] waits for the `initialize` **response** specifically.
///
/// Not a budget for indexing: a server answers `initialize` with its capabilities promptly and
/// only starts analysing afterwards.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long [`LspClient::shutdown`] waits for a graceful `shutdown` reply before terminating.
const SHUTDOWN_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
/// How long after `SIGTERM` the process tree gets to exit before `SIGKILL`. Unix only.
#[cfg_attr(not(unix), allow(dead_code))]
const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_millis(800);
/// Bound on buffered "diagnostics changed" wake signals.
///
/// A slow poller just coalesces them: each wake re-reads current state, so a dropped one loses a
/// redundant nudge, never a diagnostic.
const WAKE_CHANNEL_CAPACITY: usize = 64;

/// How long one outbound message may stall before the write is treated as failed.
///
/// Bounds *stalled* time, not total: it refreshes on every byte the peer accepts, so a large frame
/// against a slow-but-draining server never touches it. It is only consumed with the pipe full and
/// the server not reading at all - one that has not touched its stdin for this long is not slow,
/// it is not running.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bound on the un-drained custom-notification queue, past which the *oldest* entry is dropped
/// with a warning rather than silently.
///
/// Generous relative to observed traffic, so reaching it at all means something pathological.
const CUSTOM_NOTIFICATION_CAPACITY: usize = 256;

/// LSP's `ServerCancelled`: an *expected* answer to a pull request whose result would already be
/// stale, which the client is meant to retry rather than treat as a failure.
///
/// Routine, not an edge case - rust-analyzer cancels a pull sent straight after a `didChange` one
/// or two times before answering.
const SERVER_CANCELLED: i64 = -32802;
/// How many [`SERVER_CANCELLED`] responses [`LspClient::pull_diagnostics`] retries through.
///
/// An attempt cap, not a time budget - [`retry_with_deadline`] bounds wall-clock time. This only
/// stops a loop still within an unusually large budget from retrying forever.
const PULL_DIAGNOSTICS_MAX_ATTEMPTS: u32 = 20;
/// Backoff between pull retries, itself capped by whatever remains of the caller's budget - so
/// this is a ceiling, not an unconditional sleep on top of it.
const PULL_DIAGNOSTICS_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Locks a `Mutex`, recovering from poisoning so one panicking thread does not cascade into every
/// later caller. The state guarded here has no invariant a mid-operation panic could corrupt.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientUpdate {
    Updated,
    Timeout,
    Closed,
}

/// A running language server for one repository root, already past its handshake.
///
/// Every method takes `&self` and guards internally, so callers share one `Arc<LspClient>` per
/// root across every open file in it.
pub struct LspClient {
    name: &'static str,
    child: Option<Child>,
    /// Read only on unix, whose kill walks `/proc`; Windows uses the `child` handle directly.
    #[cfg_attr(not(unix), allow(dead_code))]
    pid: u32,
    exited: bool,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
    /// Bumped on every write to [`Self::diagnostics`] (push and pull paths alike), so a caller
    /// can memoize anything derived from [`Self::published_diagnostics`] and recompute only when
    /// this moves — that derivation used to run every frame (GitHub issue #471).
    diagnostics_generation: Arc<AtomicU64>,
    /// Subscribed notifications in arrival order, `params` left raw since this crate knows
    /// nothing about what any of them mean.
    ///
    /// A server can define custom notifications a client is required to act on, so dropping
    /// everything but `publishDiagnostics` would make those invisible.
    ///
    /// Bounded by [`CUSTOM_NOTIFICATION_CAPACITY`]. `publishDiagnostics` never lands here even if
    /// subscribed: it has its own structured sink, and both would give callers two disagreeing
    /// sources for one fact.
    custom_notifications: Arc<Mutex<VecDeque<(String, serde_json::Value)>>>,
    /// Which document version each [`Self::diagnostics`] entry corresponds to.
    ///
    /// Pulls are dispatched onto background threads and cannot be un-polled, so a slow response to
    /// an older edit can land after a fresher one. Every pull write is gated on this map and a
    /// stale version is discarded. The passive push path does not consult it.
    diagnostics_version: Mutex<HashMap<String, i32>>,
    /// What the server advertised in its `initialize` response, so sync and completion respect it
    /// rather than guessing. A `Mutex` because it is written from a `&self` method.
    capabilities: Mutex<ServerCapabilities>,
    /// `Mutex`-wrapped purely to make `LspClient` `Sync`: `Receiver` is `Send` but not `Sync`, and
    /// callers share one `Arc` across tasks. There is only one consumer, so it is uncontended.
    wake_rx: Mutex<Receiver<()>>,
    /// The reader thread's wake sender, kept here so a *pulled* update signals identically - a
    /// caller cannot tell push from pull, and does not need to.
    wake_tx: SyncSender<()>,
    reader_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    /// `true` until the reader thread returns, for any reason.
    ///
    /// Written from that thread through a shared `Arc`, unlike [`Self::exited`], which only
    /// `&mut self` methods set and so only reflects a deliberate shutdown. That is what makes a
    /// server crashing out from under this client observable, instead of every later request
    /// timing out one at a time with no "is this worth trying" signal.
    connection_alive: Arc<AtomicBool>,
}

/// Resolves a bare `binary` name to the absolute path [`LspClient::spawn`] hands
/// [`pty_core::new_std_command`],
/// through the same [`pty_core::resolve_on_path`] callers use to decide a server is installed.
///
/// Resolving first is load-bearing on Windows. `std::process::Command` does its own lookup there,
/// and for a bare extension-less name it only ever appends `.exe` - no `%PATHEXT%` fallback to
/// `.cmd`/`.bat`. `npm install -g` installs exactly a `.cmd` shim, so a server that resolves as
/// installed fails to spawn with `std`'s own "program not found".
///
/// Handed the resolved `...\server.cmd` path instead, `std` trusts the extension and wraps the
/// launch through `cmd.exe /c` itself - it just never discovered the file existed from the bare
/// name. Finding nothing at all is an `LspError::Spawn`, the same case `Command` would report.
fn resolve_server_binary(server: &'static str, binary: &'static str) -> Result<PathBuf, LspError> {
    resolve_server_binary_with(server, binary, pty_core::resolve_on_path)
}

/// [`resolve_server_binary`] with the resolver injected, so the not-found path is testable
/// without mutating the process-global `PATH` - which needs `unsafe` and races concurrent tests.
fn resolve_server_binary_with(
    server: &'static str,
    binary: &'static str,
    resolve: impl FnOnce(&str) -> Option<PathBuf>,
) -> Result<PathBuf, LspError> {
    resolve(binary).ok_or_else(|| LspError::Spawn {
        server,
        source: std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("`{binary}` was not found on PATH"),
        ),
    })
}

impl LspClient {
    /// Spawns `config`'s server for `repo_root` and completes the handshake, returning a client
    /// ready for use. `repo_root` must be an absolute, existing directory: a relative path has no
    /// well-formed `file://` URI.
    pub fn spawn(repo_root: &Path, config: ServerSpawnConfig) -> Result<Self, LspError> {
        if !repo_root.is_dir() {
            return Err(LspError::InvalidRoot(repo_root.to_path_buf()));
        }
        // Canonicalized so this root's URI is the same symlink-resolved path every later
        // `path_to_uri` independently arrives at for a file beneath it.
        let repo_root =
            canonicalize(repo_root).map_err(|_| LspError::InvalidRoot(repo_root.to_path_buf()))?;

        let name = config.name;
        let resolved_binary = resolve_server_binary(name, config.binary)?;
        // Through `pty_core::new_std_command`, so a `.cmd`-shim server (which `std` launches
        // via a console-subsystem `cmd.exe` host) opens no console window on Windows release
        // builds (GitHub issue #465).
        let mut command = pty_core::new_std_command(resolved_binary);
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

        // `write_message_bounded` owns its own deadline-bounded `poll` and requires a non-blocking
        // fd. Failing here would silently restore unbounded blocking writes, so it is fatal.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let fd = stdin.as_raw_fd();
            let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).map_err(|errno| {
                LspError::Io {
                    server: name,
                    source: std::io::Error::from(errno),
                }
            })?;
            let flags =
                nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
            nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags)).map_err(|errno| {
                LspError::Io {
                    server: name,
                    source: std::io::Error::from(errno),
                }
            })?;
        }
        let stdin = Arc::new(Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics: Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let diagnostics_generation = Arc::new(AtomicU64::new(0));
        let custom_notifications: Arc<Mutex<VecDeque<(String, serde_json::Value)>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let capabilities = Mutex::new(ServerCapabilities::default());
        let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(WAKE_CHANNEL_CAPACITY);
        // Cloned before the original moves into the reader thread.
        let wake_tx_for_client = wake_tx.clone();
        let connection_alive = Arc::new(AtomicBool::new(true));

        let workspace_configuration = config.workspace_configuration;
        let custom_notification_methods = config.custom_notification_methods;
        let reader_thread = std::thread::spawn({
            let pending = Arc::clone(&pending);
            let diagnostics = Arc::clone(&diagnostics);
            let diagnostics_generation = Arc::clone(&diagnostics_generation);
            let custom_notifications = Arc::clone(&custom_notifications);
            let stdin_for_replies = Arc::clone(&stdin);
            let connection_alive = Arc::clone(&connection_alive);
            move || {
                run_reader_loop(
                    stdout,
                    IncomingSinks {
                        pending,
                        diagnostics,
                        diagnostics_generation,
                        custom_notifications,
                        custom_notification_methods,
                        wake_tx,
                        stdin: stdin_for_replies,
                        workspace_configuration,
                        connection_alive,
                    },
                )
            }
        });
        // stderr is log output, not protocol - drained on its own thread so a full pipe there
        // cannot backpressure stdout, and logged rather than discarded so a startup panic shows.
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
            diagnostics_generation,
            custom_notifications,
            diagnostics_version: Mutex::new(HashMap::new()),
            capabilities,
            wake_rx: Mutex::new(wake_rx),
            wake_tx: wake_tx_for_client,
            reader_thread: Some(reader_thread),
            stderr_thread: Some(stderr_thread),
            connection_alive,
        };

        client.initialize(&repo_root, config.initialization_options)?;
        Ok(client)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

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

        // Prefers plain-text hover content. A server that honours it sends parseable text; one
        // that ignores it - typescript-language-server, pyright - is handled by the caller's own
        // degrade-to-plain-text pass rather than showing raw Markdown.
        let capabilities = ClientCapabilities {
            text_document: Some(TextDocumentClientCapabilities {
                hover: Some(HoverClientCapabilities {
                    dynamic_registration: None,
                    content_format: Some(vec![MarkupKind::PlainText]),
                }),
                // `typescript-language-server` sends no `publishDiagnostics` at all - not even an
                // empty one - until this is advertised. Harmless for servers that ignore it.
                publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                    related_information: Some(true),
                    ..Default::default()
                }),
                // A server only populates `labelDetails` once the client says it understands the
                // field, so leaving this unset means it never arrives at all.
                completion: Some(CompletionClientCapabilities {
                    completion_item: Some(CompletionItemCapability {
                        label_details_support: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            // Pyright relies on this to decide it may ask for settings rather than assume
            // defaults; `server_request_reply` answers those requests.
            workspace: Some(WorkspaceClientCapabilities {
                configuration: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        // `root_uri` is deprecated in favour of `workspace_folders`, but sent anyway:
        // `typescript-language-server`'s TypeScript-discovery walk consults it specifically and
        // fails with "Could not find a valid TypeScript installation" without it. Sending both is
        // spec-legal.
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

        let result = self.request::<lsp_types::request::Initialize>(params, INITIALIZE_TIMEOUT)?;
        // Written before `initialized` is sent, so every post-handshake caller sees it populated.
        *lock(&self.capabilities) = result.capabilities;
        self.notify::<lsp_types::notification::Initialized>(InitializedParams {})?;
        Ok(())
    }

    pub fn completion_trigger_characters(&self) -> Vec<String> {
        lock(&self.capabilities)
            .completion_provider
            .as_ref()
            .and_then(|options| options.trigger_characters.clone())
            .unwrap_or_default()
    }

    /// Whether `completionItem/resolve` is permitted. Servers commonly return only `label`/`kind`
    /// inline and expect the client to resolve the one item the user is looking at.
    pub fn supports_completion_resolve(&self) -> bool {
        lock(&self.capabilities)
            .completion_provider
            .as_ref()
            .is_some_and(|options| options.resolve_provider == Some(true))
    }

    /// Whether `textDocument/didChange` may be sent at all: `false` only for the explicit
    /// `TextDocumentSyncKind::NONE` opt-out. A server omitting the capability is assumed to
    /// permit sync.
    pub fn supports_document_sync(&self) -> bool {
        match &lock(&self.capabilities).text_document_sync {
            None => true,
            Some(TextDocumentSyncCapability::Kind(kind)) => *kind != TextDocumentSyncKind::NONE,
            Some(TextDocumentSyncCapability::Options(options)) => {
                options.change != Some(TextDocumentSyncKind::NONE)
            }
        }
    }

    /// Whether pull diagnostics are supported - not merely optional where advertised:
    /// rust-analyzer pushes `publishDiagnostics` once, after `didOpen`, and every recompute after
    /// a `didChange` must be pulled. A server without it is assumed to keep pushing.
    pub fn supports_diagnostic_pull(&self) -> bool {
        lock(&self.capabilities).diagnostic_provider.is_some()
    }

    /// `false` once the reader thread has observed this connection close, so a caller can report
    /// a dead server rather than routing requests that each time out independently.
    pub fn is_connection_alive(&self) -> bool {
        self.connection_alive.load(Ordering::SeqCst)
    }

    /// Pulls a fresh diagnostics result for `path`, retrying through [`SERVER_CANCELLED`] within
    /// `timeout` overall - not per attempt.
    ///
    /// `version` is local bookkeeping, never sent to the server: a result is only written if it
    /// is at least as new as what is already recorded, which is the only way to stop a slow pull
    /// clobbering a fresher one, since a dispatched pull cannot be un-polled.
    ///
    /// A `Full` report replaces the same entry a `publishDiagnostics` push populates and wakes the
    /// same listeners, so readers need not care which arrived. An `Unchanged` report, or one
    /// discarded as stale, is a no-op and still `Ok`: the call did get an answer, just not the
    /// newest one by the time it landed.
    pub fn pull_diagnostics(
        &self,
        path: &Path,
        version: i32,
        timeout: Duration,
    ) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        let name = self.name;
        let report_result = retry_with_deadline(
            timeout,
            PULL_DIAGNOSTICS_MAX_ATTEMPTS,
            PULL_DIAGNOSTICS_RETRY_DELAY,
            |err| matches!(err, LspError::Response { code, .. } if *code == SERVER_CANCELLED),
            |attempt_timeout| {
                self.request::<lsp_types::request::DocumentDiagnosticRequest>(
                    lsp_types::DocumentDiagnosticParams {
                        text_document: lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                        identifier: None,
                        previous_result_id: None,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    },
                    attempt_timeout,
                )
            },
            std::thread::sleep,
            || LspError::Timeout {
                server: name,
                method: lsp_types::request::DocumentDiagnosticRequest::METHOD,
            },
        )?;

        if let Some(items) = full_diagnostic_report_items(report_result) {
            let mut versions = lock(&self.diagnostics_version);
            let is_stale = versions
                .get(uri.as_str())
                .is_some_and(|&existing| version < existing);
            if !is_stale {
                versions.insert(uri.as_str().to_string(), version);
                drop(versions);
                lock(&self.diagnostics).insert(uri.as_str().to_string(), items);
                self.diagnostics_generation.fetch_add(1, Ordering::Relaxed);
                let _ = self.wake_tx.try_send(());
            }
        }
        Ok(())
    }

    /// Sends `textDocument/didOpen` for `path`. `language_id` varies by extension, not by server.
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

    /// Sends `textDocument/didChange` carrying `text` as the *entire* new document.
    ///
    /// Full-document sync deliberately, not incremental: the bare `{ text }` event variant is
    /// legal whatever sync kind was negotiated, and it makes debouncing the notification safe -
    /// an incremental stream cannot skip an intermediate delta without corrupting the server's
    /// reconstruction.
    ///
    /// `version` must be strictly greater than the last sent for `path`, but need not increase by
    /// exactly one.
    pub fn did_change_full(&self, path: &Path, text: String, version: i32) -> Result<(), LspError> {
        let uri = path_to_uri(path)?;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            }],
        };
        self.notify::<lsp_types::notification::DidChangeTextDocument>(params)
    }

    /// The most recent diagnostics for `path`. `None` means nothing has arrived yet, distinct
    /// from `Some(vec![])`, which means the file was analysed and is clean.
    pub fn diagnostics_for(&self, path: &Path) -> Option<Vec<lsp_types::Diagnostic>> {
        let uri = path_to_uri(path).ok()?;
        self.diagnostics_for_uri(&uri)
    }

    /// `true` once any result has arrived for `path`, even a clean one - which is what separates
    /// "still indexing" from "analysed, nothing to report".
    pub fn has_diagnostics_result(&self, path: &Path) -> bool {
        match path_to_uri(path) {
            Ok(uri) => self.has_diagnostics_result_uri(&uri),
            Err(_) => false,
        }
    }

    /// The `file://` [`Uri`] the lookup methods derive internally, exposed so a caller doing
    /// several lookups for one path can pay [`path_to_uri`]'s `canonicalize` once - a measured
    /// per-repaint cost otherwise.
    pub fn uri_for_path(path: &Path) -> Result<Uri, LspError> {
        path_to_uri(path)
    }

    /// The inverse of [`Self::uri_for_path`], for turning a definition result back into a path.
    ///
    /// A server can return a non-`file://` scheme - a virtual macro-expansion buffer, a library
    /// with no downloaded sources - which fails with [`LspError::InvalidUri`] rather than
    /// fabricating a path for it.
    pub fn path_for_uri(uri: &Uri) -> Result<PathBuf, LspError> {
        uri_to_path(uri)
    }

    /// The current [diagnostics](Self::published_diagnostics) generation — bumped on every
    /// stored update, so equality means "nothing derived from them can have changed".
    pub fn diagnostics_generation(&self) -> u64 {
        self.diagnostics_generation.load(Ordering::Relaxed)
    }

    pub fn diagnostics_for_uri(&self, uri: &Uri) -> Option<Vec<lsp_types::Diagnostic>> {
        lock(&self.diagnostics).get(uri.as_str()).cloned()
    }

    pub fn has_diagnostics_result_uri(&self, uri: &Uri) -> bool {
        lock(&self.diagnostics).contains_key(uri.as_str())
    }

    /// Every file with a non-empty diagnostic set - the whole-server view [`Self::diagnostics_for`]
    /// gives one file at a time.
    ///
    /// Two filters, both dropping things a caller could not use: clean files, since a list of
    /// problems has no row for one, and non-`file://` uris, which have no path to open.
    ///
    /// Sorted by path, since `HashMap` order would reshuffle the list between renders.
    pub fn published_diagnostics(&self) -> Vec<(PathBuf, Vec<lsp_types::Diagnostic>)> {
        let mut published: Vec<(PathBuf, Vec<lsp_types::Diagnostic>)> = lock(&self.diagnostics)
            .iter()
            .filter(|(_, diagnostics)| !diagnostics.is_empty())
            .filter_map(|(uri, diagnostics)| {
                let uri: Uri = uri.parse().ok()?;
                Some((uri_to_path(&uri).ok()?, diagnostics.clone()))
            })
            .collect();
        published.sort_by(|(left, _), (right, _)| left.cmp(right));
        published
    }

    /// Non-blocking: drains every buffered wake signal, returning whether any was found.
    ///
    /// Signals are per-server, not per-file, so a caller re-checks whichever file it cares about.
    pub fn drain_updates(&self) -> bool {
        let receiver = lock(&self.wake_rx);
        let mut any = false;
        while receiver.try_recv().is_ok() {
            any = true;
        }
        any
    }

    /// Non-blocking: takes every queued subscribed notification in arrival order.
    ///
    /// The whole queue moves out before this returns, so a caller doing blocking I/O with what it
    /// drained - usually the point, since such a notification needs answering - holds no lock.
    pub fn drain_custom_notifications(&self) -> Vec<(String, serde_json::Value)> {
        let mut queue = lock(&self.custom_notifications);
        queue.drain(..).collect()
    }

    /// The outbound half of [`Self::drain_custom_notifications`]: a method this crate has no type
    /// for, with `params` passed through verbatim, so the caller owns the wire shape.
    ///
    /// [`Self::notify`] stays right for anything `lsp_types` models, since it type-checks.
    pub fn notify_raw(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<(), LspError> {
        self.send_notification_raw(method, params)
    }

    /// Blocking, bounded wait for the next wake signal, for deterministic test and tooling waits.
    /// UI polling uses the non-blocking [`Self::drain_updates`].
    pub fn wait_for_update(&self, timeout: Duration) -> ClientUpdate {
        let receiver = lock(&self.wake_rx);
        match receiver.recv_timeout(timeout) {
            Ok(()) => ClientUpdate::Updated,
            Err(mpsc::RecvTimeoutError::Timeout) => ClientUpdate::Timeout,
            Err(mpsc::RecvTimeoutError::Disconnected) => ClientUpdate::Closed,
        }
    }

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
        if let Err(err) = self.write_framed(&message, method) {
            lock(&self.pending).remove(&id);
            return Err(err);
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
        self.write_framed(&message, method)
    }

    /// The single outbound path for every message, so the time bound and the stream-corruption
    /// rule live in one place rather than per call site.
    ///
    /// A write that exceeds [`WRITE_TIMEOUT`] kills the connection, for either reason:
    ///
    /// * **A partial frame reached the server.** Its framer is mid-body waiting on bytes that will
    ///   never arrive. Unrecoverable by construction.
    /// * **Not one byte was accepted.** The stream is provably intact here, so this is policy, not
    ///   correctness - and permanent, since `connection_alive` never goes back to `true`. Still
    ///   right: the budget measures *no progress*, so reaching it means a full pipe and zero bytes
    ///   for 30 seconds, which is a stopped server rather than a busy one. Reporting it as one
    ///   failed call is what let the client keep claiming to be alive while every request piled up
    ///   behind the stuck write's mutex. Callers surface this as a failure with a restart.
    ///
    /// An I/O error that wrote nothing - the usual `EPIPE` after a crash - is left alone: the
    /// reader thread's EOF is the direct signal, and arrives first.
    fn write_framed(
        &self,
        message: &serde_json::Value,
        method: &'static str,
    ) -> Result<(), LspError> {
        // Once known dead, further writes fail immediately rather than each spending a full
        // `WRITE_TIMEOUT` rediscovering it. Without this, a 3-second hover request still sat for
        // 12 seconds refilling the same wedged pipe - fanned across hover, completions and every
        // pull retry, that is the difference between reporting a dead server and appearing to hang.
        if !self.is_connection_alive() {
            return Err(LspError::ConnectionClosed { server: self.name });
        }
        let mut stdin = lock(&self.stdin);
        // Re-checked after acquiring the lock, and load-bearing rather than defensive: concurrent
        // callers all passed the check above and are queued here while one writer is mid-frame. If
        // that writer gives up part-way, a queued writer proceeding would have its own well-formed
        // frame swallowed as the previous message's body. The wedged writer publishes the death
        // before releasing the guard, so anyone holding it next sees this.
        if !self.is_connection_alive() {
            return Err(LspError::ConnectionClosed { server: self.name });
        }
        let Err(err) = transport::write_message_bounded(&mut *stdin, message, WRITE_TIMEOUT) else {
            return Ok(());
        };

        let desynced = err.stream_desynced();
        let timed_out = matches!(err, transport::BoundedWriteError::Timeout { .. });
        if timed_out || desynced {
            // Published while the guard is still held: dropping it first would let a queued writer
            // wake and write into the stream this call just desynced.
            self.connection_alive.store(false, Ordering::SeqCst);
            log::warn!(
                "marking {}'s connection dead: a `{method}` write {} within {WRITE_TIMEOUT:?}",
                self.name,
                if desynced {
                    "was cut off part-way through, desyncing the server's own framer"
                } else {
                    "was not accepted at all - the server has stopped reading its stdin"
                }
            );
        }
        drop(stdin);
        Err(if timed_out {
            LspError::Timeout {
                server: self.name,
                method,
            }
        } else {
            LspError::Io {
                server: self.name,
                source: err.into_io_error(),
            }
        })
    }

    /// Tears the session down: a best-effort `shutdown` request - an unresponsive server is not an
    /// error for teardown - then `exit`, a kill, a blocking reap, and joining the threads, which
    /// exit on their own once the process is dead. Safe to call more than once.
    pub fn shutdown(&mut self) -> Result<(), LspError> {
        if !self.exited {
            let _ = self.request::<lsp_types::request::Shutdown>((), SHUTDOWN_REQUEST_TIMEOUT);
            let _ = self.notify::<lsp_types::notification::Exit>(());

            self.kill_process_tree();
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

    /// `SIGTERM` to the process and its descendants, a grace period, then `SIGKILL`. Unix only.
    #[cfg(unix)]
    fn kill_process_tree(&mut self) {
        proc::terminate_tree(self.pid, SHUTDOWN_GRACE_PERIOD);
    }

    /// Windows equivalent: `taskkill /T` walks and terminates the whole tree synchronously
    /// (no grace period - Windows has no `SIGTERM` tier), then the direct kill as backstop.
    /// Without the tree walk a `.cmd`-shim server's real `node.exe`, and rust-analyzer's
    /// `cargo check`/`rustc` children, survive with their cwd handles inside the worktree
    /// (GitHub issues #468/#470).
    #[cfg(windows)]
    fn kill_process_tree(&mut self) {
        match pty_core::new_std_command("taskkill")
            .args(["/T", "/F", "/PID", &self.pid.to_string()])
            .output()
        {
            Ok(_) => {}
            Err(err) => log::warn!("taskkill /T /F /PID {} could not run: {err}", self.pid),
        }
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        if !self.exited {
            // `Drop` must not block, so no graceful request or grace period: straight to `SIGKILL`.
            #[cfg(unix)]
            {
                let descendants = proc::collect_descendant_pids(self.pid);
                proc::signal_pid(self.pid, nix::sys::signal::Signal::SIGKILL);
                for pid in &descendants {
                    proc::signal_pid(*pid, nix::sys::signal::Signal::SIGKILL);
                }
            }
            // Fire-and-forget `taskkill /T` (spawned, never waited - `Drop` must not block),
            // then the direct kill as backstop; see `kill_process_tree`. Stdio nulled so the
            // detached child can't hold inherited pipes open - under `cargo nextest` an
            // inherited stdout is reported as the test leaking.
            #[cfg(windows)]
            {
                let _ = pty_core::new_std_command("taskkill")
                    .args(["/T", "/F", "/PID", &self.pid.to_string()])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
                if let Some(child) = self.child.as_mut() {
                    let _ = child.kill();
                }
            }

            let reaped_immediately = self
                .child
                .as_mut()
                .and_then(|child| child.try_wait().ok().flatten());
            if reaped_immediately.is_none() {
                if let Some(mut child) = self.child.take() {
                    // `try_wait` may have run a moment before the just-killed process died, so a
                    // detached thread finishes the `wait()` - reaped, without blocking `drop`.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
            }
        }
        // Not joined here: the process is dying, so both threads see EOF and exit shortly.
        // Dropping the handles detaches the OS threads rather than blocking on them.
    }
}

/// Retry loop bounded by one deadline computed up front, each attempt taking its timeout from the
/// time *remaining* - so total elapsed time is bounded by `budget` regardless of `max_attempts`.
///
/// Giving every attempt the full `budget` instead makes the worst case `budget * max_attempts`.
///
/// A free function so the deadline arithmetic is unit-testable against a fake `attempt`, with no
/// spawned server that can be told to cancel on demand. `sleep` is a parameter for the same
/// reason: so a test need not wait out the real backoff.
fn retry_with_deadline<T>(
    budget: Duration,
    max_attempts: u32,
    retry_delay: Duration,
    is_retryable: impl Fn(&LspError) -> bool,
    mut attempt: impl FnMut(Duration) -> Result<T, LspError>,
    mut sleep: impl FnMut(Duration),
    timeout_err: impl FnOnce() -> LspError,
) -> Result<T, LspError> {
    let deadline = Instant::now() + budget;
    let mut last_err: Option<LspError> = None;
    for attempt_index in 0..max_attempts {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match attempt(remaining) {
            Ok(value) => return Ok(value),
            Err(err) if is_retryable(&err) => {
                last_err = Some(err);
                if attempt_index + 1 >= max_attempts {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                sleep(retry_delay.min(remaining));
            }
            Err(err) => return Err(err),
        }
    }
    // Every attempt was cancelled (`last_err`), or the deadline ran out before one could be tried.
    Err(last_err.unwrap_or_else(timeout_err))
}

/// `None` for an `Unchanged` report - a no-op, not an empty result - and for the `Partial` shape,
/// which a compliant server has no reason to send since this crate never asks for one.
/// Related-documents diagnostics are dropped: the model here is per-open-file, with nowhere to
/// route another file's diagnostics.
fn full_diagnostic_report_items(
    result: lsp_types::DocumentDiagnosticReportResult,
) -> Option<Vec<lsp_types::Diagnostic>> {
    match result {
        lsp_types::DocumentDiagnosticReportResult::Report(
            lsp_types::DocumentDiagnosticReport::Full(report),
        ) => Some(report.full_document_diagnostic_report.items),
        lsp_types::DocumentDiagnosticReportResult::Report(
            lsp_types::DocumentDiagnosticReport::Unchanged(_),
        ) => None,
        lsp_types::DocumentDiagnosticReportResult::Partial(_) => None,
    }
}

/// An absolute path as a percent-encoded `file://` URI, via `url` rather than hand-rolled -
/// encoding arbitrary path bytes correctly is the kind of thing that looks right until it isn't.
fn path_to_uri(path: &Path) -> Result<Uri, LspError> {
    // Best-effort, for consistency with the root URI; falls back to the path as given when the
    // file does not exist on disk.
    let canonical = canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let url = url::Url::from_file_path(&canonical)
        .map_err(|_| LspError::InvalidPath(path.to_path_buf()))?;
    url.as_str()
        .parse::<Uri>()
        .map_err(|_| LspError::InvalidPath(path.to_path_buf()))
}

/// `std::fs::canonicalize`, but on Windows simplifying the verbatim `\\?\C:\...` form back to
/// `C:\...` where safe.
///
/// `std`'s always returns the verbatim form there, and plenty of non-UNC-aware programs -
/// rust-analyzer among them - will not accept it as a working directory or in a `file://` URI.
/// A passthrough everywhere else.
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    dunce::canonicalize(path)
}

fn uri_to_path(uri: &Uri) -> Result<PathBuf, LspError> {
    let url = url::Url::parse(uri.as_str())
        .map_err(|_| LspError::InvalidUri(uri.as_str().to_string()))?;
    if url.scheme() != "file" {
        return Err(LspError::InvalidUri(uri.as_str().to_string()));
    }
    url.to_file_path()
        .map_err(|_| LspError::InvalidUri(uri.as_str().to_string()))
}

/// Everything the reader thread needs to route one incoming message, grouped into one value so
/// adding a sink does not widen two signatures in lockstep.
struct IncomingSinks {
    pending: Arc<Mutex<HashMap<i64, SyncSender<PendingResponse>>>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<lsp_types::Diagnostic>>>>,
    /// See `LspClient::diagnostics_generation` — bumped alongside every `diagnostics` write.
    diagnostics_generation: Arc<AtomicU64>,
    custom_notifications: Arc<Mutex<VecDeque<(String, serde_json::Value)>>>,
    /// See [`ServerSpawnConfig::custom_notification_methods`].
    custom_notification_methods: Vec<&'static str>,
    wake_tx: SyncSender<()>,
    stdin: Arc<Mutex<ChildStdin>>,
    workspace_configuration: WorkspaceConfigFn,
    /// Shared so the detached reply writer can report a write it could not finish, which is as
    /// fatal to the connection as EOF.
    connection_alive: Arc<AtomicBool>,
}

/// The reader thread: routes each framed message by shape - a response has `id` and no `method`,
/// a server-initiated request has both, a notification has only `method` - and exits on EOF or an
/// I/O error, clearing `connection_alive` either way.
///
/// Exiting does not attempt to reconnect. Doing so would mean re-running the handshake *and*
/// re-opening every file the caller believes is open, from a thread with no access to that state.
/// An observable dead connection is the honest answer; a caller that wants recovery can spawn a
/// fresh client the way it spawned this one.
fn run_reader_loop(stdout: std::process::ChildStdout, sinks: IncomingSinks) {
    let mut reader = BufReader::new(stdout);
    loop {
        match transport::read_message(&mut reader) {
            Ok(Some(value)) => handle_incoming(value, &sinks),
            Ok(None) => break,
            Err(err) => {
                // Logged rather than discarded: matching on `Ok(Some(_))` alone would make this
                // indistinguishable from a clean EOF.
                log::warn!("lsp-core reader thread stopping after a real I/O error: {err}");
                break;
            }
        }
    }
    // Dropping every pending sender gives threads blocked in `recv_timeout` an immediate
    // `Disconnected`, rather than each waiting out its own timeout for a response never coming.
    lock(&sinks.pending).clear();
    sinks.connection_alive.store(false, Ordering::SeqCst);
}

fn handle_incoming(value: serde_json::Value, sinks: &IncomingSinks) {
    let Some(object) = value.as_object() else {
        return;
    };

    if let Some(id) = object.get("id") {
        if object.contains_key("method") {
            // Answered generically with `null` rather than left unanswered, except
            // `workspace/configuration`, which `server_request_reply` answers properly.
            let method = object.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let reply = server_request_reply(
                id,
                method,
                object.get("params"),
                sinks.workspace_configuration,
            );

            // Written from a detached thread, not inline: if the child's stdin buffer is full,
            // writing here would stop this thread draining its stdout - and a child blocked
            // writing to that undrained stdout while waiting for stdin deadlocks both sides.
            // Replies are rare, so the per-call thread is negligible.
            let stdin = Arc::clone(&sinks.stdin);
            // This thread takes the mutex the whole client serializes on, so it must follow both
            // halves of `write_framed`'s rule: sitting here for a full `WRITE_TIMEOUT` against a
            // wedged server would park every caller behind it. Skip if already known dead,
            // re-check once the guard is held, and publish a death before releasing it.
            let connection_alive = Arc::clone(&sinks.connection_alive);
            let method = method.to_string();
            std::thread::spawn(move || {
                if !connection_alive.load(Ordering::SeqCst) {
                    return;
                }
                let mut guard = lock(&stdin);
                if !connection_alive.load(Ordering::SeqCst) {
                    return;
                }
                let Err(err) = transport::write_message_bounded(&mut *guard, &reply, WRITE_TIMEOUT)
                else {
                    return;
                };
                let desynced = err.stream_desynced();
                // Matches `write_framed`'s condition rather than dying on any error: an `EPIPE`
                // that wrote nothing means the process is gone, and EOF says that better.
                if desynced || matches!(err, transport::BoundedWriteError::Timeout { .. }) {
                    connection_alive.store(false, Ordering::SeqCst);
                    log::warn!(
                        "marking a connection dead: replying to a server-initiated `{method}` \
                         failed ({err:?}); desynced={desynced}"
                    );
                }
            });
            return;
        }

        // A response to one of our own requests.
        let Some(id) = id.as_i64() else { return };
        let sender = lock(&sinks.pending).remove(&id);
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

    // `publishDiagnostics` goes to its typed sink; a subscribed method is queued verbatim;
    // anything else is ignored.
    let Some(method) = object.get("method").and_then(|m| m.as_str()) else {
        return;
    };
    if method == lsp_types::notification::PublishDiagnostics::METHOD {
        let Some(params) = object.get("params") else {
            return;
        };
        let parse_result =
            serde_json::from_value::<lsp_types::PublishDiagnosticsParams>(params.clone());
        let Ok(parsed) = parse_result else {
            log::warn!("failed to parse a real publishDiagnostics payload: {parse_result:?}");
            return;
        };
        lock(&sinks.diagnostics).insert(parsed.uri.as_str().to_string(), parsed.diagnostics);
        sinks.diagnostics_generation.fetch_add(1, Ordering::Relaxed);
        let _ = sinks.wake_tx.try_send(());
        return;
    }

    if !sinks.custom_notification_methods.contains(&method) {
        return;
    }

    {
        let mut queue = lock(&sinks.custom_notifications);
        while queue.len() >= CUSTOM_NOTIFICATION_CAPACITY {
            let dropped = queue.pop_front();
            log::warn!(
                "dropping the oldest un-drained custom notification ({:?}) - {} are already \
                 queued, which is this client's real cap; nothing appears to be draining them",
                dropped.map(|(method, _)| method),
                CUSTOM_NOTIFICATION_CAPACITY
            );
        }
        queue.push_back((
            method.to_string(),
            object
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        ));
    }
    // The same wake `publishDiagnostics` sends, reused rather than a second channel: a caller's
    // existing poll loop already drains on a wake, so this needs no new machinery.
    let _ = sinks.wake_tx.try_send(());
}

/// Builds a reply to one server-initiated request.
///
/// `workspace/configuration` is special-cased: its result type is one entry per requested item,
/// so a bare `null` is not a legal shape for it even where a server tolerates one. Each entry
/// comes from the server's own [`ServerSpawnConfig::workspace_configuration`].
///
/// Every other method keeps the generic `null` result, which stays legal where result types are
/// optional.
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

/// The stderr-draining thread, so a full stderr pipe cannot backpressure the process. Each line
/// is logged rather than discarded, so a startup failure stays observable.
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
    use std::process::Command;
    use std::time::Instant;
    // Only `IncomingHarness` uses this, and it is `#[cfg(unix)]` - the import has to carry the
    // same gate or it is an unused import on Windows.
    #[cfg(unix)]
    use test_support::ChildGuard;

    #[test]
    fn canonicalize_resolves_a_real_directory() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let resolved = canonicalize(dir.path()).expect("a real, existing directory must resolve");
        assert!(resolved.is_dir());
        assert!(resolved.is_absolute());
        // Same directory identity as std's answer - without demanding std's *spelling*, which
        // on Windows is the verbatim `\\?\C:\...` form this function exists to avoid.
        assert_eq!(
            std::fs::canonicalize(&resolved).expect("std canonicalize of the resolved path"),
            std::fs::canonicalize(dir.path()).expect("std canonicalize")
        );
        #[cfg(windows)]
        assert!(
            !resolved.to_string_lossy().starts_with(r"\\?\"),
            "the verbatim prefix breaks file:// URIs and must be simplified away, got {resolved:?}"
        );
    }

    #[test]
    fn canonicalize_reports_a_real_error_for_a_missing_path() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        assert!(canonicalize(&missing).is_err());
    }

    /// A minimal cargo project in a fresh tempdir, with no dependencies - so workspace discovery
    /// needs no network and indexes fast.
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

    /// The rust-analyzer config these tests spawn against, kept local since `lsp-core` must
    /// stand alone.
    fn rust_analyzer_config() -> ServerSpawnConfig {
        ServerSpawnConfig {
            name: "rust-analyzer",
            binary: "rust-analyzer",
            args: Vec::new(),
            initialization_options: None,
            workspace_configuration: default_workspace_configuration,
            custom_notification_methods: Vec::new(),
        }
    }

    #[test]
    fn resolve_server_binary_uses_whatever_the_injected_resolver_finds() {
        let expected = PathBuf::from("C:\\fake\\typescript-language-server.cmd");
        let resolved = resolve_server_binary_with(
            "typescript-language-server",
            "typescript-language-server",
            |name| {
                assert_eq!(name, "typescript-language-server");
                Some(expected.clone())
            },
        )
        .expect("the injected resolver found a real path");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_server_binary_is_a_real_honest_spawn_error_when_nothing_is_found() {
        let err =
            resolve_server_binary_with("rust-analyzer", "rust-analyzer", |_| None).unwrap_err();
        match err {
            LspError::Spawn { server, source } => {
                assert_eq!(server, "rust-analyzer");
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected LspError::Spawn, got {other:?}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_real_windows_batch_shim_is_unspawnable_by_bare_name_but_spawns_via_its_own_resolved_path()
    {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let script = dir.path().join("lsp_core_fake_server.cmd");
        std::fs::write(&script, "@echo off\r\necho ready\r\n").expect("write real .cmd script");

        // This tempdir is deliberately not on `PATH`, so a bare name must fail the same way an
        // `.exe`-only lookup would against a `.cmd`-only install: `NotFound`, nothing else.
        let bare_name_result = Command::new("lsp_core_fake_server").output();
        let bare_name_err = bare_name_result.expect_err(
            "a bare name must never find a sibling .cmd file - this is the real bug being fixed",
        );
        assert_eq!(bare_name_err.kind(), std::io::ErrorKind::NotFound);

        // An absolute path carrying the `.cmd` extension lets `std`'s own batch-file detection
        // run it through `cmd.exe /c`.
        let output = Command::new(&script)
            .output()
            .expect("a real .cmd file must spawn successfully once given its own resolved path");
        assert!(
            output.status.success(),
            "the real script must exit successfully"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("ready"),
            "the real script's own stdout must actually be captured"
        );
    }

    /// Whether a `GotoDefinitionResponse` carries zero locations, so a poller can tell "not
    /// resolved yet" from a real empty answer. `Scalar` has no empty state.
    fn goto_definition_response_is_empty(response: &lsp_types::GotoDefinitionResponse) -> bool {
        match response {
            lsp_types::GotoDefinitionResponse::Scalar(_) => false,
            lsp_types::GotoDefinitionResponse::Array(locations) => locations.is_empty(),
            lsp_types::GotoDefinitionResponse::Link(links) => links.is_empty(),
        }
    }

    /// Direct `/proc/<pid>` existence check, to prove a process is really gone. Unix only.
    #[cfg(unix)]
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

    /// The real collaborators [`handle_incoming`] needs, built without spawning a language server:
    /// a genuine `ChildStdin` (taken from a real, trivial `cat` child, since `ChildStdin` has no
    /// constructor of its own and the notification branches under test never write to it), plus
    /// the same real `Arc<Mutex<..>>` sinks [`LspClient::spawn`] wires up. Returns the child too,
    /// so the caller keeps it alive - and kills it - for the duration of the test.
    #[cfg(unix)]
    struct IncomingHarness {
        /// Kept alive for the test's duration, and killed on drop by the guard itself.
        _child: ChildGuard,
        sinks: IncomingSinks,
        wake_rx: Receiver<()>,
    }

    #[cfg(unix)]
    impl IncomingHarness {
        /// `subscribed` is the real [`ServerSpawnConfig::custom_notification_methods`] list this
        /// harness's `handle_incoming` calls are driven with.
        fn new(subscribed: &[&'static str]) -> Self {
            let mut child = ChildGuard::spawn(
                Command::new("cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null()),
            )
            .expect("spawning a real `cat` for its stdin handle");
            let stdin = child.stdin.take().expect("piped stdin");
            let (wake_tx, wake_rx) = mpsc::sync_channel::<()>(WAKE_CHANNEL_CAPACITY);
            Self {
                _child: child,
                sinks: IncomingSinks {
                    pending: Arc::new(Mutex::new(HashMap::new())),
                    diagnostics: Arc::new(Mutex::new(HashMap::new())),
                    diagnostics_generation: Arc::new(AtomicU64::new(0)),
                    custom_notifications: Arc::new(Mutex::new(VecDeque::new())),
                    custom_notification_methods: subscribed.to_vec(),
                    wake_tx,
                    stdin: Arc::new(Mutex::new(stdin)),
                    workspace_configuration: default_workspace_configuration,
                    connection_alive: Arc::new(AtomicBool::new(true)),
                },
                wake_rx,
            }
        }

        fn feed(&self, message: serde_json::Value) {
            handle_incoming(message, &self.sinks);
        }

        fn drain_custom(&self) -> Vec<(String, serde_json::Value)> {
            lock(&self.sinks.custom_notifications).drain(..).collect()
        }
    }

    /// The real capability `crate::client`'s notification branch gained so a server's own custom
    /// protocol extension stops being invisible: a subscribed method is queued verbatim, method
    /// and raw params both intact, in real arrival order.
    #[cfg(unix)]
    #[test]
    fn a_subscribed_notification_is_queued_verbatim_for_draining() {
        let harness = IncomingHarness::new(&["tsserver/request", "server/other"]);
        harness.feed(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tsserver/request",
            "params": [[1, "_vue:projectInfo", { "file": "/tmp/App.vue" }]],
        }));
        harness.feed(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "server/other",
            "params": { "token": "t" },
        }));

        let drained = harness.drain_custom();
        assert_eq!(
            drained.len(),
            2,
            "every subscribed notification should be queued, in arrival order"
        );
        assert_eq!(drained[0].0, "tsserver/request");
        assert_eq!(
            drained[0].1,
            serde_json::json!([[1, "_vue:projectInfo", { "file": "/tmp/App.vue" }]]),
            "the raw params must survive verbatim - this crate deliberately doesn't interpret them"
        );
        assert_eq!(drained[1].0, "server/other");
        assert!(
            harness.drain_custom().is_empty(),
            "draining should leave the queue genuinely empty, not re-yield the same entries"
        );
    }

    /// The real "no added cost for the servers that were already working" guarantee: a client that
    /// subscribed to nothing (rust-analyzer, typescript-language-server, pyright - all three of
    /// this app's pre-existing servers) queues nothing at all, no matter how much unrelated
    /// notification traffic its server produces. Behaviorally identical to before this capability
    /// existed.
    #[cfg(unix)]
    #[test]
    fn a_client_that_subscribed_to_nothing_queues_nothing_at_all() {
        let harness = IncomingHarness::new(&[]);
        for method in ["$/progress", "window/logMessage", "tsserver/request"] {
            harness.feed(serde_json::json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": { "anything": true },
            }));
        }
        assert!(
            harness.drain_custom().is_empty(),
            "an un-subscribed method must be ignored exactly as it always was, not queued for a              caller that will never read it"
        );
        assert!(
            harness.wake_rx.try_recv().is_err(),
            "and it must not even cost a spurious wake signal"
        );
    }

    /// Subscription is per-method, not all-or-nothing: an un-subscribed method is still ignored
    /// even on a client that subscribed to something else.
    #[cfg(unix)]
    #[test]
    fn an_unsubscribed_method_is_ignored_even_when_other_methods_are_subscribed() {
        let harness = IncomingHarness::new(&["tsserver/request"]);
        harness.feed(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "$/progress",
            "params": { "token": "t" },
        }));
        assert!(harness.drain_custom().is_empty());
    }

    /// The other half of the partition: `publishDiagnostics` keeps going only to its own real,
    /// typed sink and must never *also* appear on the custom-notification path, which would give
    /// callers two disagreeing sources for the same real data.
    #[cfg(unix)]
    #[test]
    fn publish_diagnostics_never_appears_on_the_custom_notification_path() {
        let harness = IncomingHarness::new(&["textDocument/publishDiagnostics"]);
        harness.feed(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/main.rs",
                "diagnostics": [{
                    "range": {
                        "start": { "line": 1, "character": 4 },
                        "end": { "line": 1, "character": 5 },
                    },
                    "severity": 1,
                    "message": "mismatched types",
                }],
            },
        }));

        assert!(
            harness.drain_custom().is_empty(),
            "publishDiagnostics has its own real sink and must not be duplicated onto the \
             generic custom-notification queue"
        );
        let diagnostics = lock(&harness.sinks.diagnostics);
        let recorded = diagnostics
            .get("file:///tmp/main.rs")
            .expect("the real, typed diagnostics sink should still have been populated");
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].message, "mismatched types");
    }

    /// Both paths send the same real wake signal, so the `app` crate's single existing poll loop
    /// notices either without new polling machinery.
    #[cfg(unix)]
    #[test]
    fn a_custom_notification_sends_the_same_real_wake_signal_diagnostics_do() {
        let harness = IncomingHarness::new(&["tsserver/request"]);
        assert!(
            harness.wake_rx.try_recv().is_err(),
            "no wake should be pending before anything arrives"
        );
        harness.feed(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tsserver/request",
            "params": [[1, "_vue:projectInfo", {}]],
        }));
        assert!(
            harness.wake_rx.try_recv().is_ok(),
            "a subscribed notification should wake a poller the same way a real \
             publishDiagnostics push does"
        );
    }

    /// A server that sends a subscribed notification faster than anything drains it must not grow
    /// this queue without limit - the *oldest* entry is dropped, so the newest, most relevant ones
    /// are the ones that survive.
    #[cfg(unix)]
    #[test]
    fn the_custom_notification_queue_is_bounded_and_drops_the_oldest_first() {
        let harness = IncomingHarness::new(&["server/custom"]);
        let total = CUSTOM_NOTIFICATION_CAPACITY + 10;
        for index in 0..total {
            harness.feed(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "server/custom",
                "params": { "index": index },
            }));
        }

        let drained = harness.drain_custom();
        assert_eq!(
            drained.len(),
            CUSTOM_NOTIFICATION_CAPACITY,
            "the queue must stay capped rather than growing for the life of the process"
        );
        assert_eq!(
            drained[0].1["index"],
            serde_json::json!(total - CUSTOM_NOTIFICATION_CAPACITY),
            "the oldest entries are the ones dropped, so the newest survive"
        );
        assert_eq!(
            drained[drained.len() - 1].1["index"],
            serde_json::json!(total - 1)
        );
    }

    /// A server-initiated *request* (an `id` alongside the `method`) still takes the real
    /// reply path and must not leak onto the notification queue, which has no way to answer one.
    #[cfg(unix)]
    #[test]
    fn a_server_initiated_request_is_not_treated_as_a_custom_notification() {
        let harness = IncomingHarness::new(&["client/registerCapability"]);
        harness.feed(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "client/registerCapability",
            "params": { "registrations": [] },
        }));
        assert!(
            harness.drain_custom().is_empty(),
            "a request needs a real reply, not to be queued as an unanswerable notification"
        );
    }

    #[cfg(unix)]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
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
        // Captured before `shutdown()`, since reading it afterwards races reparenting. Caveat:
        // this dependency-free fixture may spawn no descendants at all in the time available, so
        // the assertion can pass over an empty list - kept because it costs nothing.
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

    #[cfg(unix)]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
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

    #[ignore = "external: rust-analyzer; see docs/testing.md"]
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
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
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
        assert_eq!(
            mismatch.range.start.line, 1,
            "expected the mismatch diagnostic's range to point at the real offending line, \
             got: {mismatch:#?}"
        );

        client.shutdown().expect("shutdown should succeed");
    }

    /// The real, live proof Revision R8.5b's `LspClient::did_change_full`/`LspClient::
    /// pull_diagnostics` exist to deliver: a real rust-analyzer, opened against a clean file,
    /// gets a real *unsaved* edit (via `did_change_full` alone - no `did_open`/re-spawn, no file
    /// ever written to disk) that introduces a genuine `E0308` type mismatch, and a real,
    /// specifically *pulled* `textDocument/diagnostic` request reports it.
    ///
    /// This is the direct, load-bearing regression test for a real, live-discovered protocol
    /// fact this crate's original design got wrong: a real, installed rust-analyzer was found,
    /// by live probing while building this feature, to publish `publishDiagnostics` via *push*
    /// only once - immediately after `didOpen` - and never again on its own initiative after a
    /// subsequent `didChange`, despite advertising `textDocumentSync` support for it; real,
    /// updated diagnostics must be actively *pulled* instead (see `LspClient::
    /// supports_diagnostic_pull`'s own docs). An earlier version of this test (and of the `app`
    /// crate's own end-to-end wiring test) asserted purely on the *push* sink
    /// (`Self::diagnostics_for`) after a `did_change_full` call and hung for the full real 60s+
    /// deadline every time - a genuine, live-reproduced correctness gap this fix closes, not a
    /// hypothetical one.
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    #[test]
    fn did_change_full_then_a_real_pull_reports_a_real_new_diagnostic() {
        let project = write_scratch_project(
            "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n",
        );
        let main_rs = project.path().join("src").join("main.rs");
        let source = std::fs::read_to_string(&main_rs).expect("read fixture source");

        let client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");

        client
            .did_open(&main_rs, source, 1, "rust")
            .expect("didOpen should send successfully");

        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            if client.has_diagnostics_result(&main_rs) {
                break;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "rust-analyzer never published a real baseline result within 180s"
            );
            match client.wait_for_update(remaining.min(Duration::from_secs(5))) {
                ClientUpdate::Updated | ClientUpdate::Timeout => continue,
                ClientUpdate::Closed => panic!("closed before a real baseline result arrived"),
            }
        }
        assert!(
            client
                .diagnostics_for(&main_rs)
                .is_some_and(|diagnostics| diagnostics.is_empty()),
            "sanity check: the unedited fixture should have a real, clean baseline"
        );
        assert!(
            client.supports_diagnostic_pull(),
            "sanity check: this test's whole point is proving the real pull path rust-analyzer \
             actually needs - if this ever goes false, rust-analyzer stopped advertising \
             diagnostic_provider and this test's premise needs re-checking against its real, \
             current behavior"
        );

        let edited_content = "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n\nfn bad() -> i32 {\n    \"not a number\"\n}\n".to_string();
        client
            .did_change_full(&main_rs, edited_content, 2)
            .expect("did_change_full should send successfully");

        // An active pull, not a wait on the push sink, which never fires again here.
        //
        // `pull_diagnostics` retries `ServerCancelled` itself, but a *successful* pull can still
        // report a stale empty result when reanalysis has not caught up - a different race, with
        // no per-response "is this done" signal to retry on. Hence this outer bounded re-pull.
        let pull_deadline = Instant::now() + Duration::from_secs(60);
        let diagnostics = loop {
            client
                .pull_diagnostics(&main_rs, 2, Duration::from_secs(30))
                .expect(
                    "a real pull_diagnostics call should eventually succeed, retrying through \
                     any real ServerCancelled responses",
                );
            let diagnostics = client
                .diagnostics_for(&main_rs)
                .expect("pull_diagnostics should have populated a real result");
            if !diagnostics.is_empty() {
                break diagnostics;
            }
            assert!(
                Instant::now() < pull_deadline,
                "no real, non-empty diagnostics result arrived from repeated real pulls within \
                 60s of the genuine new type mismatch being sent"
            );
            std::thread::sleep(Duration::from_millis(300));
        };
        let mismatch = diagnostics.iter().find(|diagnostic| {
            let message = diagnostic.message.to_lowercase();
            message.contains("mismatched")
                || (message.contains("expected") && message.contains("i32"))
        });
        assert!(
            mismatch.is_some(),
            "expected a real diagnostic referencing the genuine type mismatch, got: \
             {diagnostics:#?}"
        );
        let mismatch = mismatch.expect("checked above");
        assert_eq!(
            mismatch.severity,
            Some(lsp_types::DiagnosticSeverity::ERROR),
            "a genuine type mismatch should be reported at ERROR severity, got: {mismatch:#?}"
        );
        assert_eq!(
            mismatch.range.start.line, 6,
            "expected the mismatch diagnostic's range to point at the real offending line, \
             got: {mismatch:#?}"
        );
    }

    /// Revision R8.5b audit finding 5's direct regression test: a real, *late-arriving* pull
    /// result tagged with an older document version must never clobber a real, *already-landed*
    /// result for a newer one - the exact race `LspClient::diagnostics_version` exists to close
    /// (see that field's own docs). Reproduced against a real rust-analyzer, not simulated: a
    /// real `did_change_full` introduces a genuine type error, a real pull at version 10 records
    /// it, then a real second pull against the *same, still-erroring* live content is issued but
    /// deliberately mislabeled with version 3 (lower than what's already recorded) - standing in
    /// for "this response, though arriving now, actually corresponds to an older edit that was
    /// slow to answer". Real `pull_diagnostics` must still return `Ok(())` (a real answer *was*
    /// obtained, just discarded as stale) but must not overwrite the real version-10 result: the
    /// diagnostics this call left in place are checked directly by pre-emptively clearing what's
    /// there (via a version-0 sync-independent probe is not available, so a distinguishing
    /// baseline is used instead - see inline comments) rather than merely re-observing identical
    /// content.
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    #[test]
    fn a_stale_lower_version_pull_never_clobbers_an_already_landed_newer_one() {
        let project = write_scratch_project(
            "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n",
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
            if client.has_diagnostics_result(&main_rs) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "rust-analyzer never published a real baseline result within 180s"
            );
            client.wait_for_update(Duration::from_secs(5));
        }

        let bad_content = "fn main() {\n    let x: i32 = 1;\n    println!(\"{}\", x);\n}\n\nfn bad() -> i32 {\n    \"not a number\"\n}\n".to_string();
        client
            .did_change_full(&main_rs, bad_content, 10)
            .expect("did_change_full should send successfully");

        let pull_deadline = Instant::now() + Duration::from_secs(60);
        let baseline_diagnostics = loop {
            client
                .pull_diagnostics(&main_rs, 10, Duration::from_secs(30))
                .expect("a real pull_diagnostics call should eventually succeed");
            let diagnostics = client
                .diagnostics_for(&main_rs)
                .expect("pull_diagnostics should have populated a real result");
            if !diagnostics.is_empty() {
                break diagnostics;
            }
            assert!(
                Instant::now() < pull_deadline,
                "no real, non-empty diagnostics arrived from repeated real pulls within 60s"
            );
            std::thread::sleep(Duration::from_millis(300));
        };
        assert!(
            !baseline_diagnostics.is_empty(),
            "sanity check: the real version-10 pull should have recorded the genuine mismatch"
        );

        // Pull #2, same content, deliberately mislabelled version 3 - standing in for a slow
        // response to an older edit landing after a fresher one.
        client
            .pull_diagnostics(&main_rs, 3, Duration::from_secs(30))
            .expect(
                "a stale-version pull should still return Ok(()) - a real answer was genuinely \
                 obtained, it's just discarded as stale, not a failure of the call itself",
            );

        // The stored result must be untouched. Even with identical content, the version staying
        // at 10 rather than regressing to 3 is what keeps a later version-11 pull from being
        // wrongly treated as stale.
        let after_stale_pull = client
            .diagnostics_for(&main_rs)
            .expect("a real result should still be present");
        assert_eq!(
            after_stale_pull, baseline_diagnostics,
            "a real pull tagged with an older document version must never clobber the real \
             result already recorded for a newer one"
        );

        // Proof the version did not regress: a pull at 5 must also be discarded. Had the stale
        // version-3 pull regressed it, 5 would wrongly count as newer.
        client
            .pull_diagnostics(&main_rs, 5, Duration::from_secs(30))
            .expect("a stale-version pull should still return Ok(())");
        let after_second_stale_pull = client
            .diagnostics_for(&main_rs)
            .expect("a real result should still be present");
        assert_eq!(
            after_second_stale_pull, baseline_diagnostics,
            "the recorded version must not have regressed after the first stale pull - a \
             version-5 pull (still lower than the real version-10 result already recorded) must \
             also be discarded"
        );

        client.shutdown().expect("shutdown should succeed");
    }

    #[cfg(unix)]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    #[test]
    fn killing_the_real_process_flips_is_connection_alive_to_false() {
        let project = write_scratch_project("fn main() {}\n");
        let client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("spawning + initializing rust-analyzer should succeed");
        assert!(
            client.is_connection_alive(),
            "a freshly spawned, initialized client should report its connection as alive"
        );

        let descendants = proc::collect_descendant_pids(client.pid);
        proc::signal_pid(client.pid, nix::sys::signal::Signal::SIGKILL);
        for pid in &descendants {
            proc::signal_pid(*pid, nix::sys::signal::Signal::SIGKILL);
        }

        let deadline = Instant::now() + Duration::from_secs(10);
        while client.is_connection_alive() {
            assert!(
                Instant::now() < deadline,
                "is_connection_alive() should have flipped to false within 10s of the real \
                 process being killed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    #[test]
    fn a_real_but_frozen_server_fails_the_write_within_the_budget_instead_of_hanging_forever() {
        let project = write_scratch_project("fn main() {\n    let x: i32 = 1;\n}\n");
        let main_rs = project.path().join("src").join("main.rs");
        let client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("a real rust-analyzer should spawn and handshake");
        let pid = client.pid;
        assert!(
            client.is_connection_alive(),
            "sanity check: a freshly handshaked client is alive"
        );

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGSTOP,
        )
        .expect("a real SIGSTOP against the real rust-analyzer pid");

        // Well past a pipe's ~64 KiB buffer, so the write cannot complete - the shape of a
        // whole-file sync for a large source file.
        let oversized = "x".repeat(256 * 1024);
        let started = Instant::now();
        let result = client.did_change_full(&main_rs, oversized.clone(), 2);
        let elapsed = started.elapsed();

        // Resumed before any assertion can unwind past it: a still-stopped process ignores the
        // `SIGTERM` half of teardown and lingers until the `SIGKILL`.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGCONT,
        );

        assert!(
            matches!(result, Err(LspError::Timeout { .. })),
            "a write a frozen server never accepts must surface as a real timeout, got: {result:?}"
        );
        assert!(
            elapsed < WRITE_TIMEOUT * 2,
            "the write must give up on its own budget rather than blocking indefinitely - took \
             {elapsed:?} against a {WRITE_TIMEOUT:?} budget"
        );
        assert!(
            !client.is_connection_alive(),
            "a write that could not be completed leaves the server's own framer mid-message; the \
             connection must be reported dead rather than left looking healthy - this reporting \
             `true` is exactly what made the original bug invisible"
        );
    }

    #[cfg(unix)]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    #[test]
    fn a_connection_already_known_dead_fails_further_writes_immediately() {
        let project = write_scratch_project("fn main() {}\n");
        let main_rs = project.path().join("src").join("main.rs");
        let client = LspClient::spawn(project.path(), rust_analyzer_config())
            .expect("a real rust-analyzer should spawn and handshake");
        let pid = client.pid;

        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("a real SIGKILL against the real rust-analyzer pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while client.is_connection_alive() {
            assert!(
                Instant::now() < deadline,
                "the real process death should have been observed within 10s"
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        let started = Instant::now();
        let result = client.request::<lsp_types::request::HoverRequest>(
            lsp_types::HoverParams {
                text_document_position_params: lsp_types::TextDocumentPositionParams {
                    text_document: lsp_types::TextDocumentIdentifier {
                        uri: path_to_uri(&main_rs).expect("a real uri"),
                    },
                    position: lsp_types::Position {
                        line: 0,
                        character: 3,
                    },
                },
                work_done_progress_params: Default::default(),
            },
            Duration::from_secs(30),
        );
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(LspError::ConnectionClosed { .. })),
            "a request on a known-dead connection must say so directly, got: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "it must not re-pay any real timeout budget to rediscover a death already recorded - \
             took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
    #[test]
    fn a_writer_queued_behind_one_that_gives_up_is_refused_rather_than_corrupting_the_stream() {
        let project = write_scratch_project("fn main() {}\n");
        let main_rs = project.path().join("src").join("main.rs");
        let client = Arc::new(
            LspClient::spawn(project.path(), rust_analyzer_config())
                .expect("a real rust-analyzer should spawn and handshake"),
        );
        let pid = client.pid;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGSTOP,
        )
        .expect("a real SIGSTOP against the real rust-analyzer pid");

        let wedged = {
            let client = Arc::clone(&client);
            let main_rs = main_rs.clone();
            std::thread::spawn(move || client.did_change_full(&main_rs, "x".repeat(256 * 1024), 2))
        };

        // Writer B: started while A is still inside its write, so it queues on the mutex rather
        // than racing the liveness check.
        std::thread::sleep(Duration::from_secs(2));
        let queued = {
            let client = Arc::clone(&client);
            let main_rs = main_rs.clone();
            std::thread::spawn(move || {
                let started = Instant::now();
                let result = client.did_change_full(&main_rs, "fn main() {}".to_string(), 3);
                (result, started.elapsed())
            })
        };

        let wedged_result = wedged.join().expect("writer A should not panic");
        let (queued_result, queued_elapsed) = queued.join().expect("writer B should not panic");
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGCONT,
        );

        assert!(
            matches!(wedged_result, Err(LspError::Timeout { .. })),
            "sanity check: writer A should have given up on its own budget, got {wedged_result:?}"
        );
        assert!(
            matches!(queued_result, Err(LspError::ConnectionClosed { .. })),
            "the queued writer must be refused outright once the writer ahead of it desynced the \
             stream - anything else means it wrote a frame the peer will swallow as the previous \
             message's body: {queued_result:?}"
        );
        assert!(
            queued_elapsed < WRITE_TIMEOUT * 2,
            "the queued writer must be refused as soon as the lock is released, not left to time \
             out on its own account - took {queued_elapsed:?}"
        );
    }

    /// The typescript-language-server config these tests spawn against, kept local as
    /// [`rust_analyzer_config`] is.
    fn typescript_language_server_config() -> ServerSpawnConfig {
        ServerSpawnConfig {
            name: "typescript-language-server",
            binary: "typescript-language-server",
            args: vec!["--stdio".to_string()],
            initialization_options: None,
            workspace_configuration: default_workspace_configuration,
            custom_notification_methods: Vec::new(),
        }
    }

    /// A minimal TypeScript project in a fresh tempdir, plus a live `npm install typescript@5`.
    ///
    /// The install is required, not cautious: `typescript-language-server` bundles no TypeScript
    /// and refuses to initialize without a discoverable one, and a global `typescript@7` - the Go
    /// rewrite, with no classic `lib/tsserver.js` - does not satisfy it either.
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
    #[ignore = "external: typescript-language-server; see docs/testing.md"]
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
    #[ignore = "external: typescript-language-server; see docs/testing.md"]
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
        // Pinned rather than narrated: this client asks for `PlainText`, and
        // typescript-language-server sends `Markdown` anyway - the case a caller's
        // degrade-to-plain-text fallback exists for.
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

    /// The pyright-langserver config these tests spawn against, kept local as the others are.
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
            custom_notification_methods: Vec::new(),
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
    #[ignore = "external: pyright-langserver; see docs/testing.md"]
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
        // Pyright names the literal's inferred type (`Literal['not a number']`), not `str`, so
        // this matches the wording actually seen rather than a guessed-at substring.
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
    #[ignore = "external: pyright-langserver; see docs/testing.md"]
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
            canonicalize(&path).expect("canonicalize"),
            "converting a real path to a URI and back should yield the same real, canonical path"
        );
        // `LspClient::path_for_uri` is a thin wrapper over the same logic - confirm
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
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
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
        // Line 8, not 7: the blank line the fixture's literal inserts between the two `fn`s
        // shifts everything below it down one. Character 20 lands inside `add_one`.
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
    #[ignore = "external: rust-analyzer; see docs/testing.md"]
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
        // Line 5 is `    let result = add_one(41);`; character 20 is inside the
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

        let deadline = Instant::now() + Duration::from_secs(180);
        let response = loop {
            match client.request::<lsp_types::request::GotoDefinition>(
                params.clone(),
                Duration::from_secs(10),
            ) {
                // A `Some`-but-empty response means "not resolved yet", not "no definition" -
                // this fixture has one by construction - so keep polling as for `None`.
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

        // An untagged three-way union. rust-analyzer replies with `Array`, but matching all three
        // avoids depending on that.
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
        // `fn add_one` starts at line 0 here, so the range must land there rather than at the
        // call site it was requested from.
        assert_eq!(
            range.start.line, 0,
            "expected the real definition location to point at the real `fn add_one` line, got: \
             {range:?}"
        );

        client.shutdown().expect("shutdown should succeed");
    }
}

/// Fast coverage for [`retry_with_deadline`]'s deadline arithmetic, with no language server: a
/// fake `attempt` consumes 80% of whatever timeout it is given and always asks to retry.
///
/// Real clocks and sleeps, just a test-scale budget, so the whole thing runs in under a second.
#[cfg(test)]
mod retry_deadline_tests {
    use super::*;

    fn fake_cancelled() -> LspError {
        LspError::Response {
            server: "fake",
            method: "textDocument/diagnostic",
            code: SERVER_CANCELLED,
            message: "server cancelled the request".to_string(),
        }
    }

    #[test]
    fn total_real_elapsed_time_stays_within_the_caller_budget_not_multiplied_by_attempt_count() {
        let budget = Duration::from_millis(200);
        let max_attempts = 20;
        let start = Instant::now();

        let result: Result<(), LspError> = retry_with_deadline(
            budget,
            max_attempts,
            Duration::from_millis(0),
            |_err| true, // always retryable, mirroring a persistent cancel.
            |remaining: Duration| -> Result<(), LspError> {
                // 80% of whatever window it is given: each attempt looks individually reasonable
                // while the total grows without bound as they accumulate.
                std::thread::sleep(remaining.mul_f64(0.8));
                Err(fake_cancelled())
            },
            std::thread::sleep,
            fake_cancelled,
        );

        let elapsed = start.elapsed();
        assert!(
            result.is_err(),
            "every attempt was made retryable, so this should exhaust"
        );
        assert!(
            elapsed < budget * 3,
            "real total elapsed time ({elapsed:?}) should stay close to the real budget \
             ({budget:?}), not grow toward budget * max_attempts ({:?}) the way the pre-fix \
             per-attempt-gets-the-full-budget design did",
            budget * max_attempts
        );
    }

    #[test]
    fn a_successful_first_attempt_returns_immediately() {
        let start = Instant::now();
        let result = retry_with_deadline(
            Duration::from_secs(5),
            20,
            Duration::from_millis(100),
            |_err: &LspError| true,
            |_remaining: Duration| -> Result<&'static str, LspError> { Ok("ready") },
            std::thread::sleep,
            fake_cancelled,
        );
        assert!(matches!(result, Ok("ready")));
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "a first-attempt success should not wait on any real deadline/backoff machinery"
        );
    }

    #[test]
    fn a_non_retryable_error_is_returned_immediately_without_retrying() {
        let attempts = std::cell::Cell::new(0);
        let result: Result<(), LspError> = retry_with_deadline(
            Duration::from_secs(5),
            20,
            Duration::from_millis(0),
            |_err| false, // nothing is retryable.
            |_remaining: Duration| -> Result<(), LspError> {
                attempts.set(attempts.get() + 1);
                Err(LspError::Timeout {
                    server: "fake",
                    method: "textDocument/diagnostic",
                })
            },
            std::thread::sleep,
            fake_cancelled,
        );
        assert!(matches!(result, Err(LspError::Timeout { .. })));
        assert_eq!(
            attempts.get(),
            1,
            "a non-retryable error must not trigger a second attempt"
        );
    }
}
