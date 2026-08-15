# Crate map

Scope / Owns / Does not own, for every crate in the workspace today, plus the one planned addition.

## `wt-core`

**Scope.** Git operations against a worktree: enumerate, diff, merge, rebase, undo/redo, blame,
stage, remote sync. Reads go through `gix`; writes go through the real `git` CLI with explicit argv
(never an interpolated shell string).

**Owns.** All git domain types (`WorktreeDiff`, `MergeOutcome`, `Graph`, `RebaseOutcome`, …) and the
functions that produce them. Every public function is blocking by contract — a GUI caller is
expected to offload it to a background executor.

**Does not own.** Any UI concern, any process/PTY concern, any LSP concern. Zero `gpui` dependency,
verified — the only two mentions of `gpui` in this crate are comments explaining why one isn't taken.

## `pty-core`

**Scope.** Spawning and driving a PTY-backed child process (`portable-pty`), and nothing about what
happens to the bytes that come out of it.

**Owns.** `PtySession`, `SpawnOptions`, process lifecycle (`kill`, `pause`, `resume`, `try_wait`).
Output is exposed as a plain `std::sync::mpsc::Receiver<Vec<u8>>`.

**Does not own.** ANSI/terminal-grid parsing (that's `crates/app/src/terminal/`), any git concern,
any gpui dependency.

## `lsp-core`

**Scope.** A Language Server Protocol client: spawn, initialize, request/notify, diagnostics.

**Owns.** `LspClient` and its full request surface (`did_open`, `pull_diagnostics`,
`completion_trigger_characters`, …), the `Content-Length` transport framing, and `WorkspaceConfigFn`
— currently the only dependency-injection seam in the workspace.

**Does not own.** Process resolution beyond `resolve_on_path`, for which it takes a path dependency
on `pty-core`. Zero gpui dependency.

## `app`

**Scope.** The GPUI desktop application: rendering, window/focus/keymap management, and
orchestration of the three core crates. The only crate with a `[[bin]]` target (`src/main.rs`,
packaged as `jerry` on release).

**Owns.** Everything visual, plus — today, and not by design — roughly 24k lines of code with no
`gpui` dependency at all: `hooks/` (the Claude-hook HTTP side-channel), `text_history.rs`,
`provenance/`, parts of `rail/`, `sidebar/fold_state.rs`, `work_surface/tab_order_state.rs`, and
more. See [`overview.md`](./overview.md) for why this is debt rather than design, and the extraction
plan for `hooks/`.

**Does not own.** Nothing is currently off-limits, which is exactly the problem: `render.rs` files
call into `wt-core` directly (109 times in `graph_view/render.rs` alone) and one even shells out to
`git` itself (`sidebar/render.rs:6534`) instead of going through a Command. Fixing this is tracked
work, not done in this pass.

## `jerry-cli` (planned, not yet created)

**Scope.** A `clap` binary dispatching the same `Command`/`Query` types the GPUI view uses.

**Owns.** Argument parsing and output formatting only. No business logic — if logic ends up here
that the view also needs, it belongs in the application layer instead.

**Does not own.** Any `gpui` dependency, ever. This crate is the proof that the application layer
is genuinely UI-agnostic; if building it needs a `gpui` import, the application layer isn't done.

**Prerequisite:** the two mechanical blockers in `overview.md` (glob imports, adapter calls from
`render.rs`) — this crate can't exist meaningfully until the Command/Query surface it would dispatch
actually exists.
