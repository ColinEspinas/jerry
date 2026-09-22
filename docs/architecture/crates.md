# Crate map

Scope / Owns / Does not own, for every crate in the workspace today, plus the one planned addition.

## `jerry-git`

**Scope.** Git operations against a worktree: enumerate, diff, merge, rebase, undo/redo, blame,
stage, remote sync. Reads go through `gix`; writes go through the real `git` CLI with explicit argv
(never an interpolated shell string).

**Owns.** All git domain types (`WorktreeDiff`, `MergeOutcome`, `Graph`, `RebaseOutcome`, …) and the
functions that produce them. Every public function is blocking by contract — a GUI caller is
expected to offload it to a background executor.

**Does not own.** Any UI concern, any process/PTY concern, any LSP concern. Zero `gpui` dependency,
verified — the only two mentions of `gpui` in this crate are comments explaining why one isn't taken.

## `jerry-pty`

**Scope.** Spawning and driving a PTY-backed child process (`portable-pty`), and nothing about what
happens to the bytes that come out of it — plus the workspace's "how children are spawned on this
OS" helpers that aren't PTY-specific (`resolve_on_path`, `new_std_command`).

**Owns.** `PtySession`, `SpawnOptions`, process lifecycle (`kill`, `pause`, `resume`, `try_wait`).
Output is exposed as a plain `std::sync::mpsc::Receiver<Vec<u8>>`. Also `new_std_command`, the one
sanctioned constructor for every non-PTY `std::process::Command` in the workspace — it suppresses
the per-spawn console window on Windows GUI-subsystem release builds (decisions.md §10).

**Does not own.** ANSI/terminal-grid parsing (that's `crates/jerry-app/src/terminal/`), any git concern,
any gpui dependency.

## `jerry-core`

**Scope.** The contract every client and the host share: the `Command`/`Query` traits with
`Invocability` and `Locality`, `Ctx`/`Caller`, the `Report` projection with stable error codes, the
JSON-RPC 2.0 frame codec, the per-repository host registry, and a blocking socket client. The
Git-locality Command and Query implementations live here so standalone `jerry-cli` can run them.

**Owns.** No threads, no listener, no sessions. Every variant a client can send is catalogued in
`request.rs` and pinned by a JSON fixture under `fixtures/`.

**Does not own.** Dispatch, the listener and the session table (`jerry-host`); anything `gpui`.

## `jerry-host`

**Scope.** The session host: the one place a `Call` is authorized and executed. A dispatch
thread woken by a channel, the AF_UNIX listener with a reader and writer per connection, the
session table (`crate::session::SessionManager`, every PTY it spawned or is tracking - agents
and plain terminal tabs alike), and notification fan-out to every connected client. In-process
inside `jerry-app` through stage 2; its own process at stage 3.

**Owns.** Caller classification (an env-injected agent id the host itself handed out, or a
human), `Invocability` and cwd confinement, `event/*` push. The session table and the real
`jerry_pty::PtySession` behind each session it spawns (`SessionSpawn`/`SessionResize`/
`SessionKill`/`SessionsQuery`, decisions.md §23) - `AgentTable` is now a thin view over it, kept
for its pre-existing callers. The data-plane adapter (`SessionHandle`): an in-process byte
stream and `write_input`, handed out directly, never through `Call`/`Report`.

**Does not own (yet).** The wire contract and the Git-locality implementations (`jerry-core`);
any rendering, anything `gpui`. The hook store (still `jerry-app`'s `hooks/store.rs`) and
`jerry-app`'s own production pane-spawn path, which does not yet dispatch `SessionSpawn` -
decisions.md §23 names the remaining work.

## `jerry-lsp`

**Scope.** A Language Server Protocol client: spawn, initialize, request/notify, diagnostics.

**Owns.** `LspClient` and its full request surface (`did_open`, `pull_diagnostics`,
`completion_trigger_characters`, …), the `Content-Length` transport framing, and `WorkspaceConfigFn`
— currently the only dependency-injection seam in the workspace.

**Does not own.** Process resolution beyond `resolve_on_path`, for which it takes a path dependency
on `jerry-pty`. Zero gpui dependency.

## `jerry-app`

**Scope.** The GPUI desktop application: rendering, window/focus/keymap management, and
orchestration of the three core crates. The only crate with a `[[bin]]` target (`src/main.rs`,
packaged as `jerry` on release).

**Owns.** Everything visual, plus — today, and not by design — roughly 24k lines of code with no
`gpui` dependency at all: `hooks/` (the Claude-hook HTTP side-channel), `text_history.rs`,
`provenance/`, parts of `rail/`, `sidebar/fold_state.rs`, `work_surface/tab_order_state.rs`, and
more. See [`overview.md`](./overview.md) for why this is debt rather than design, and the extraction
plan for `hooks/`.

**Does not own.** Nothing is currently off-limits, which is exactly the problem: `render.rs` files
call into `jerry-git` directly (109 times in `graph_view/render.rs` alone) and one even shells out to
`git` itself (`sidebar/render.rs:6534`) instead of going through a Command. Fixing this is tracked
work, not done in this pass.

## `jerry-cli`

**Scope.** The `jerry` command: what agents and humans type. Deliberately shallow. `clap` builds a
`Request`, the transport decides whether a live Jerry serves this repository (`JERRY_HOST_SOCKET`,
then `--instance`, then the registry), the `Report` becomes an exit code and output. Git-locality
requests run in-process when no host serves the repository; anything else needs one.

**Owns.** Argument parsing, the bi-mode choice as a pure function, the exit-code contract (0 done,
1 action required, 2 usage, 3 refused, 4 no instance, 5 failed), and output: JSON only with
`--json`, prose otherwise, diagnostics always on stderr. The whole binary is tested through
`jerry_cli::run` without spawning it.

**Does not own.** Any logic the GUI also needs (that is `jerry-core`), any `gpui`.
