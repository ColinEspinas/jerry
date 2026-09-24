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
the per-spawn console window on Windows GUI-subsystem release builds (decisions.md §10). And
`new_detached_command` (decisions.md §14/§24): the one place `CREATE_BREAKAWAY_FROM_JOB`
(Windows) / a fresh process group (unix) is set, so a `jerry-host` process can outlive the
job-jobbed app or CLI that started it — used by `jerry_core::host_spawn` and, from `jerry-host`'s
own `main.rs`, the self-adoption on the other side of that spawn. Every call in this crate is a
safe `std` wrapper — no `unsafe`. *Detecting* a forbidding job needs the real Win32
`IsProcessInJob`/`QueryInformationJobObject` read, which stays out of this crate entirely and
lives in `jerry-app`'s and `jerry-host`'s own `job_object.rs` instead (CLAUDE.md's unsafe list),
injected into `jerry_core::host_spawn::spawn_or_connect_with` by whichever of those calls it.

**Does not own.** ANSI/terminal-grid parsing (that's `crates/jerry-app/src/terminal/`), any git concern,
any gpui dependency, and (deliberately) any `unsafe` code at all.

## `jerry-core`

**Scope.** The contract every client and the host share: the `Command`/`Query` traits with
`Invocability` and `Locality`, `Ctx`/`Caller`, the `Report` projection with stable error codes, the
JSON-RPC 2.0 frame codec, the per-repository host registry, and a blocking socket client. The
Git-locality Command and Query implementations live here so standalone `jerry-cli` can run them.

**Owns.** No threads, no listener, no sessions. Every variant a client can send is catalogued in
`request.rs` and pinned by a JSON fixture under `fixtures/`. `crate::host_spawn::spawn_or_connect`
(decisions.md §24): resolve the registry for a repository, connect to a live version-matched
host, flag a version mismatch, or spawn `jerry-host` detached (claiming the right to via
`Registry::claim`, so two racing callers cannot both spawn one) and wait for its descriptor - the
one implementation both `jerry host start` and (once wired) `jerry-app`'s own `HostRuntime` call,
which is why this crate takes a real (non-dev) dependency on `jerry-pty`. `crate::jerry_binary::
locate_named` finds any sibling binary this workspace ships (`jerry`, `jerry-host`). No `unsafe`
here either - detecting a job that forbids `CREATE_BREAKAWAY_FROM_JOB` is injected in by the
caller (`jerry-app`'s or `jerry-host`'s own `job_object.rs`), never called directly.

**Does not own.** Dispatch, the listener and the session table (`jerry-host`); anything `gpui`;
any `unsafe` code.

## `jerry-host`

**Scope.** The session host: the one place a `Call` is authorized and executed. A dispatch
thread woken by a channel, the AF_UNIX listener with a reader and writer per connection, the
session table (`crate::session::SessionManager`, every PTY it spawned or is tracking - agents
and plain terminal tabs alike), and notification fan-out to every connected client. Its own
process from decisions.md §24 (`[[bin]] jerry-host`, `src/main.rs`) - a real, tested binary that
self-registers in the host registry, listens, and runs until idle or told to stop
(`Host::run_lifecycle`). `jerry-app`'s own production dispatch does not yet spawn-or-connect to
it, though (§24's own "what did not move") - `HostRuntime` still only ever constructs an
in-process `Host`, unchanged since decisions.md §23.

**Owns.** Caller classification (an env-injected agent id the host itself handed out, or a
human), `Invocability` and cwd confinement, `event/*` push - now gated behind an explicit
`event/subscribe` request rather than automatic on connect (§24), so a one-off request/response
client does not count as "connected" for `Host::run_lifecycle`'s own idle check. The session
table and the real `jerry_pty::PtySession` behind each session it spawns (`SessionSpawn`/
`SessionResize`/`SessionKill`/`SessionsQuery`, decisions.md §23) - `AgentTable` is now a thin view
over it, kept for its pre-existing callers. The data-plane adapter (`SessionHandle`): an
in-process byte stream and `write_input`, handed out directly, never through `Call`/`Report` -
still in-process only; the per-session socket #507 adds is not built yet.

**Does not own (yet).** The wire contract and the Git-locality implementations (`jerry-core`);
any rendering, anything `gpui`. The hook store (still `jerry-app`'s `hooks/store.rs`) and
`jerry-app`'s own production pane-spawn path, which does not yet dispatch `SessionSpawn` -
decisions.md §23 names the remaining work. Nor does `jerry-app` yet spawn-or-connect to this
process at all in production - decisions.md §24 names that remaining work too.

## `jerry-lsp`

**Scope.** A Language Server Protocol client: spawn, initialize, request/notify, diagnostics.

**Owns.** `LspClient` and its full request surface (`did_open`, `pull_diagnostics`,
`completion_trigger_characters`, …), the `Content-Length` transport framing, and `WorkspaceConfigFn`
— currently the only dependency-injection seam in the workspace.

**Does not own.** Process resolution beyond `resolve_on_path`, for which it takes a path dependency
on `jerry-pty`. Zero gpui dependency.

## `jerry-app`

**Scope.** The GPUI desktop application: rendering, window/focus/keymap management, and
orchestration of the three core crates. The only crate with a `gpui` `[[bin]]` target
(`src/main.rs`, packaged as `Jerry`/`jerry-app` on release) - `jerry-cli` and `jerry-host` each
ship their own non-`gpui` binary too.

**Owns.** Everything visual, plus — today, and not by design — roughly 24k lines of code with no
`gpui` dependency at all: `hooks/` (the Claude-hook HTTP side-channel), `text_history.rs`,
`provenance/`, parts of `rail/`, `sidebar/fold_state.rs`, `work_surface/tab_order_state.rs`, and
more. See [`overview.md`](./overview.md) for why this is debt rather than design, and the extraction
plan for `hooks/`.

**Does not own.** Nothing is currently off-limits, which is exactly the problem: `render.rs` files
call into `jerry-git` directly (109 times in `graph_view/render.rs` alone) and one even shells out to
`git` itself (`sidebar/render.rs:6534`) instead of going through a Command. Fixing this is tracked
work, not done in this pass.

## `jerry-ui`

**Scope.** Jerry's design-system crate (decisions.md §27): the semantic theming tier (`Theme`,
non-colour scales, `StateStyle`) and a small set of reusable `RenderOnce` components (`Button`,
`IconButton`, `Banner`, `ListRow`, `Badge`, `Divider`). The second, and only other, crate in this
workspace allowed to depend on `gpui`.

**Owns.** Its own token schema (`crate::schema::ThemeSchema`) for validating a user theme file into
a `Theme`. Every component takes a `Theme` by value from its caller and paints a real
`debug_selector` for `VisualTestContext::debug_bounds` - no global theme state, no styling decision
baked in that a caller can't override.

**Does not own.** Any `jerry-app` state, settings, or file I/O; any of the other core crates
(`jerry-git`/`jerry-pty`/`jerry-lsp`/`jerry-core`/`jerry-host`) - `jerry-app` constructs a `Theme`
from its own resolved palette and passes it in, rather than this crate reaching outward for one.
Behaviour gpui-kit does well (focus ring, keyboard activation) is borrowed as a documented pattern,
never as a dependency; no `gpui-component`/gpui-kit code is vendored here today.

## `jerry-cli`

**Scope.** The `jerry` command: what agents and humans type. Deliberately shallow. `clap` builds a
`Request`, the transport decides whether a live Jerry serves this repository (`JERRY_HOST_SOCKET`,
then `--instance`, then the registry), the `Report` becomes an exit code and output. Git-locality
requests run in-process when no host serves the repository; anything else needs one. `jerry host
start`/`jerry host stop` (decisions.md §24) are the exception to "deliberately shallow": `start`
calls `jerry_core::host_spawn::spawn_or_connect` directly rather than going through a `Request` at
all, since spawning a process is not a Command/Query.

**Owns.** Argument parsing, the bi-mode choice as a pure function, the exit-code contract (0 done,
1 action required, 2 usage, 3 refused, 4 no instance, 5 failed), and output: JSON only with
`--json`, prose otherwise, diagnostics always on stderr. The whole binary is tested through
`jerry_cli::run` without spawning it.

**Does not own.** Any logic the GUI also needs (that is `jerry-core`), any `gpui`.
