# Architecture decisions

Why Jerry's target architecture ([`overview.md`](./overview.md), [`crates.md`](./crates.md)) looks
the way it does — the reasoning and rejected alternatives, not just the rule. `CLAUDE.md` and the
`architecture` skill state each rule in a line or two and point here for the argument behind it,
deliberately, so they stay short.

This is a **decisions log, not a running narrative** — the distinction that matters after this
project's own `BUILD-LOG.md` (592 KB, a growing chronicle nobody could tell was current) got
deleted for exactly that reason (§4 below). A decision here is written once. If a later decision
changes an earlier one, it gets its **own new numbered entry** below, and the old entry's Status
line is updated to point at it — never edited back to "current." Add a new entry only for a real
decision (a new crate boundary, a new cross-cutting rule, a reversal) — not for every routine
application of one that already exists here.

## 1. Core crates stay free of `gpui`

**Status:** Accepted.

**Context:** `jerry-git`, `jerry-pty`, and `jerry-lsp` were already built with no `gpui` dependency —
verified: the only two occurrences of the string `gpui` in those three crates are comments, one
about a version pin, the other noting that a real GPUI fake-clock test isn't possible without one.
This wasn't written down anywhere, so it was one dependency addition away from silently breaking.

**Decision:** No crate other than `crates/jerry-app` (and the planned `crates/jerry-cli`, which must
never gain one either) may depend on `gpui` or `gpui_platform`. This is the foundation the rest of
the target architecture is built on: it's what makes a headless CLI over the same domain logic
possible at all.

**Consequences:** A PR adding `gpui` to `jerry-git`/`jerry-pty`/`jerry-lsp`'s `Cargo.toml` is a hard
reject, not a design discussion. Any type crossing from a core crate into `crates/jerry-app` and back
must be plain data — never a `gpui::Context`, `Window`, or similar (`crates/jerry-app/src/work_surface/agents.rs`
violates this today by taking `Context<AdeApp>` directly in agent-lifecycle methods; tracked as
follow-up, not retroactively blessed). `jerry-lsp`'s one cross-crate dependency (`jerry-pty`, for
`resolve_on_path`) staying a path dependency between two gpui-free crates is fine and doesn't need
repeating elsewhere.

## 2. Commands and queries, not loose functions, as the application-layer unit

**Status:** Accepted.

**Context:** `jerry-git` exposes its capabilities as loose, well-named functions —
`commit_all_changes`, `attempt_merge`, `discard_worktree`, `resolve_hunk`, and about forty more.
Clean, but no shared shape: each has its own argument list and result type, so nothing can dispatch
them generically. That matters because the same action needs to be triggerable from the GPUI view
*and* from a future `crates/jerry-cli`, and actions need to compose (a merge that discards on
conflict, a commit that also pushes) without one caller knowing the other's calling convention.

Two shapes were considered. **Plain hexagonal — application services**: group the existing
functions into service structs behind port traits. Standard, but a service method is still just a
function with extra ceremony — it doesn't give the CLI and the view a common thing to dispatch, or
composition a unit to compose. **Command + Query, reifying every action as a value**: each
mutation becomes a `Command` with a typed input struct and typed outcome; each read becomes a
`Query`. One dispatch function serves any caller that can construct the input.

**Decision:** Adopt Command + Query.

```rust
pub trait Command {
    type Outcome;
    fn validate(&self, ctx: &Ctx) -> Result<(), ValidationError>;
    fn execute(self, ctx: &Ctx) -> Result<Self::Outcome, Error>;
}
```

`jerry-git`'s existing functions map onto this almost one-to-one — `commit_all_changes` becomes
`CommitAllChanges { paths: Vec<PathBuf> } -> CommitAllChangesOutcome`, and so on. The
transformation is mechanical: the logic inside each function doesn't change, only its calling
convention does.

**Consequences:** `crates/jerry-cli` becomes possible without duplicating logic — it constructs the
same `Command` values the view does and calls the same `execute`. New capabilities are added as new
`Command`/`Query` types starting now, even though the existing `jerry-git` functions aren't
retrofitted in this pass. This is deliberately *not* a command bus with an execution log — undo/redo
and provenance tracking already exist as their own hand-built mechanisms (`jerry-git::undo`,
`crates/jerry-app/src/provenance/`); layering a generic event-sourced bus on top would duplicate them for
no immediate benefit. A real need for a unified execution log would be its own new entry here, not
an assumption baked into this one.

## 3. The view dispatches commands and queries; it never calls an adapter directly

**Status:** Accepted. Partially enforced — see Consequences.

**Context:** `crates/jerry-app`'s render layer currently calls straight into `jerry-git` and, in one place,
straight into a raw process spawn: `graph_view/render.rs` alone has 109 `jerry_git::` references,
`sidebar/render.rs` has 33, and `sidebar/render.rs:6534` shells out to
`std::process::Command::new("git")` directly, bypassing `jerry-git` entirely. Several `render.rs`
files also call `cx.background_spawn`/`cx.spawn` directly around adapter calls, duplicating the
offload-to-background decision ad hoc at every call site.

This works today because `crates/jerry-app` is the only consumer of `jerry-git`. It becomes a real problem
the moment a second consumer (`crates/jerry-cli`) exists: behavior implemented as "whatever the
render function happens to do around the `jerry_git::` call" isn't available to the CLI, and a bug
fixed in the view's copy has to be separately remembered in the CLI's.

**Decision:** Render code (`render.rs`, anything returning `impl IntoElement` or implementing
`Render`) may only read state already held on `AdeApp` and dispatch a `Command`/`Query`, rendering
the outcome. It may never call `jerry_git::`, `jerry_pty::`, `jerry_lsp::`, or `std::process::Command`
directly. The background-spawn decision moves into the dispatch layer, once, instead of being
repeated at every call site.

**Consequences:** This is currently violated at scale (204 direct adapter references across
`render.rs` files, per `.claude/conventions-baseline.json`) and is **not** retroactively fixed by
this decision — it's the target; the gap is tracked as GitHub issues. New render code must not add
new adapter calls, effective immediately, and this is now **mechanically checked**, not just
reviewed against: `.claude/hooks/check-conventions.sh` greps every `render.rs` file for
`jerry_git::`/`jerry_pty::`/`jerry_lsp::`/`process::Command::new` and fails — in the pre-commit hook and
in CI — if the count exceeds the checked-in baseline. It's a textual ratchet (the count may only go
down), not a type-aware lint, and it needs no prerequisite. A full `clippy::disallowed-methods`
version, scoped to `render.rs` files, is still blocked on cleaning up `use super::*` globs first:
today a glob means a lint can't reliably tell which module a symbol resolved from — that cleanup is
tracked as its own issue, and the ratchet is the interim mechanical backstop until it lands.

## 4. `BUILD-LOG.md`/`ASSESSMENT.md` retired in favor of this decisions log

**Status:** Accepted.

**Context:** `BUILD-LOG.md` (592 KB, 7,715 lines) was a hand-written, append-only narrative
changelog of early build sessions. `ASSESSMENT.md` (24.7 KB) was a one-time, end-of-build
retrospective. Both were treated as living documentation — `CONTRIBUTING.md` told every new
contributor to read both before starting and mandated updating `BUILD-LOG.md` "alongside real
functional changes." Neither was current: `BUILD-LOG.md`'s last commit predated 241 of the
repository's 587 commits (~41% of project history undocumented in the file CONTRIBUTING called the
design record), and `ASSESSMENT.md` had exactly one commit — its own creation — and said so about
itself, opening by calling itself a stale snapshot left as-is rather than rewritten. Both files'
style — long narrative prose, revision numbers (R1–R12, R8.5a), design-history justification inline
with the artifact it documents — is also the pattern `CLAUDE.md`'s comment rule now excludes from
source comments; keeping the files around as the "proper place" for that material would have
undermined that rule immediately.

**Decision:** Delete both files. Git history retains every word for anyone who wants the
archaeology. Their replacement is this decisions log: entries written once, not maintained as a
running log, explicitly not a substitute for `git log`.

**Consequences:** `CONTRIBUTING.md`'s instructions to read and update `BUILD-LOG.md` are removed.
`README.md`'s `## Status` states current status directly rather than deferring to `ASSESSMENT.md`.
Design decisions worth recording going forward get a new numbered entry above, not an appended
paragraph in a long-running file.

## 5. `gix` for reads; the `git` CLI where git's own output format is the product

**Status:** Accepted.

**Context:** `jerry-git` has both `gix` and `std::process::Command` available to it, and the choice
was being re-argued per function, inline, in module comments. The two are not interchangeable.
`gix` is a library over the object database and refs; it has no formatter that reproduces
`git diff`'s unified-diff text (hunk headers, rename and binary detection, and working-tree state
blended in), and `gix-diff` works on tree and blob objects rather than the working tree.

**Decision:** Reads that ask the object database or the ref store a structured question — resolving
`HEAD`, finding a reference, computing a merge-base, walking commits — go through `gix`. Anything
whose *product* is git's own text or whose semantics live in the porcelain — the unified diff,
`ls-files`, `stash`, `worktree remove`, index manipulation — shells out to the `git` CLI, with an
explicit argument vector (never an interpolated string) and with any config it depends on pinned
via `-c`.

**Consequences:** Reimplementing `git diff`'s output format on `gix-diff` primitives is out of
scope, and a PR proposing it needs to argue with this entry first. Shelling out means the invocation
owns its own correctness: pin the config the parser assumes (`diff.mnemonicPrefix`,
`diff.noprefix`, `core.quotePath`), validate any object id reaching an argument vector as hex, and
treat stderr on a successful command as noise rather than failure.

## 6. One place answers "what does this worktree contain"

**Status:** Accepted.

**Context:** Two features hand-rolled worktree enumeration independently and both tripped on the
same directory: a recursive `fs::read_dir` walk behind the search panel, and an unconditional
`git add -A` behind review snapshots. A gitignored build directory dominates a real checkout — this
repository's own `target/` is the large majority of its files — and a filesystem walk must open and
`stat` all of it before discovering there was nothing to search, because ignore matching happens
after descent. Git's happens before it.

**Decision:** `jerry-git::worktree_files` is the single answer, built on `git ls-files --cached
--others --exclude-standard`. New callers use it rather than growing a third walk.

**Consequences:** "Content" means what git would show: tracked paths stay listed even under a
later-added ignore rule, and untracked paths appear only if git would offer to stage them. A caller
that must keep working outside a git repository owns its own fallback, since this returns an error
there rather than an empty list.

## 7. Interactive rebase is driven through git's own editor hooks, not reimplemented

**Status:** Accepted; amended 2026-09-22 (issue #501) to point `GIT_SEQUENCE_EDITOR`/`GIT_EDITOR`
at the `jerry` binary instead of a generated `/bin/sh` script, making this Unix-restriction gone.
Rebase's own Commands are §20.

**Context:** Jerry needs `git rebase --interactive`'s six todo verbs without an interactive
terminal. The alternative to driving real git is reimplementing the todo machinery on plumbing
(`cherry-pick`, `commit --amend`, `reset`) — which means re-deriving conflict handling, `squash`
message combination, `REBASE_HEAD`/`stopped-sha` bookkeeping, and resume-after-restart semantics
from scratch.

**Decision:** Drive the real `git rebase -i` non-interactively through the same environment hooks
a human's `$EDITOR` is invoked through — `GIT_SEQUENCE_EDITOR` and `GIT_EDITOR` point at hidden
`jerry git-sequence-editor <todo-file>` and `jerry git-editor <message-file>` subcommands
(`crates/jerry-cli/src/lib.rs`), never a generated shell script. `git-sequence-editor` copies the
prepared todo (`jerry_git::rebase::write_plan_state`'s `todo.txt`) over the one git generated;
`git-editor` classifies each invocation *by the content of the message file git hands it*, never
by invocation order, via a pure function (`jerry_git::rebase::classify_editor_invocation`):

1. First line `# This is a combination of ...` → a `squash` combination; accept unmodified.
2. Contains `You are currently editing a commit` → a `reword`; pop the next slot from a persisted
   message queue. Nothing queued means exit non-zero, which reproduces `edit`'s stop exactly.
3. Anything else → a conflict-resumed step; accept git's pre-filled message and **do not** advance
   the queue cursor.

Case 3 is not optional. A conflict-resumed `pick` goes through git's ordinary `commit` codepath and
does open the editor; treating that as a `reword` consumes a message meant for a later row.
`jerry_git::rebase::start_interactive_rebase` takes the `jerry` binary's path as a parameter — its
own caller locates it (`crates/jerry-app/src/host.rs::find_jerry_binary` in the app,
`std::env::current_exe` in the CLI) — and persists the exact `GIT_EDITOR` value it built into the
sidecar (`editor-command`), so a later `--continue`/`--skip` (`run_rebase_step`) reconstructs the
same value without needing that path again, including after a process restart.

**Consequences:** Sidecar state (todo file, reword-message queue, cursor, the persisted
`editor-command` value, and a plan cross-reference) lives under `<git-dir>/ade-rebase/`, resolved
per-worktree rather than in the shared common dir, and survives until the rebase completes or
aborts — so a stop can be reconstructed after a process restart, including whether a row was
`edit` or a message-less `reword`, which git alone cannot distinguish once stopped. The queue is
plain files, not JSON, so `git-editor` needs no parser beyond what it already has. Env var values
are spliced unquoted into a shell command line by git — confirmed still true with `jerry` as the
target, including on Windows, where Git for Windows' own bundled `sh` runs the same `sh -c`
dance — so every embedded path is still POSIX-single-quoted
(`jerry_git::rebase::shell_single_quote`), for both `sh` and Windows. This mechanism now runs on
every platform the `jerry` binary does; the rebase tests that exercise it (`jerry-git`'s
`tests/rebase_editor.rs`, `jerry-core`'s `tests/rebase_commands.rs`) are no longer Unix-only.
Conflicts are never auto-resolved or rolled back, matching `crate::rewrite`.

## 8. `jerry-pty` owns spawning only; `alacritty_terminal` stays in `crates/jerry-app`

**Status:** Accepted.

**Context:** Upstream Zed drives `alacritty_terminal::tty::Pty` directly and lets its `EventLoop`
own a thread that both pumps bytes and feeds the `Term` grid parser — one composition, not
separable into a standalone spawn primitive.

**Decision:** `jerry-pty` owns spawn, raw-byte output, resize and kill via `portable-pty`, and knows
nothing about ANSI escapes or grid state. `crates/jerry-app` owns the `Term` grid, driven by the bytes
this crate streams.

**Consequences, all load-bearing:**

- **The output channel is bounded.** An undrained unbounded channel is an unbounded leak —
  measured at ~40MB/s of RSS growth against a `yes` pipe. A full `sync_channel` blocks the reader's
  `send`, so it stops calling `read`, the kernel pty buffer fills, and the child's `write` blocks:
  ordinary terminal backpressure.
- **Shutdown is a self-pipe, not a dropped master fd.** `try_clone_reader()` hands back an
  independently `dup`'d fd, so dropping `master` does not unblock the reader. An earlier version
  only appeared to work because `take_writer()`'s `Drop` writes `\n` + EOT, which local echo bounced
  back and incidentally woke the read — with `stty -echo`, the thread leaked for the process's life.
- **Kill signals the process group *and* a `/proc` descendant walk.** `portable-pty` calls `setsid`,
  so `killpg` reaches ordinary descendants, but anything calling `setsid` itself escapes it. The
  descendant set is snapshotted *before* signalling, because reading it afterwards races the kernel
  reparenting a dying process's children.
- **`Drop` never blocks; `shutdown()` is the deterministic one.** `Drop` signals and does one
  non-blocking `try_wait`, handing any unreaped child to a detached thread — a multi-hundred-ms
  freeze here would freeze the GPUI thread. `shutdown()` blocks until the tree is dead and reaped.
- **Input goes through a writer thread.** A full pty write buffer would otherwise block whichever
  thread called `write_input`, plausibly a key handler on the main thread.

**Windows is narrower** (originally reasoned from `portable-pty` 0.9.0 and
`filedescriptor` 0.8.3 sources plus `cargo check --target x86_64-pc-windows-gnu`; now exercised on
real hardware — see issues #465–#468). `kill()`/`shutdown()` terminate the whole tree via
`taskkill /T` — the no-`unsafe` alternative to job objects; best-effort against re-parented
descendants, with the direct kill as backstop (an orphaned tree was how npm `.cmd`-shim agents'
real `node.exe` survived, #468). There is no self-pipe either: `WSAPoll` accepts only sockets
and a ConPTY master is a named pipe, so the reader blocks until `master` itself drops, *not* when
the child is reaped - the reader's own EOF must never be read as "the child exited" on this
platform. `portable-pty` exposes no way to interrupt that blocked read from another thread either
(no overlapped I/O, no `CancelIoEx`/`CancelSynchronousIo` equivalent - verified against
`portable-pty-0.9.0/src/win/{conpty,psuedocon}.rs`, which read/close synchronously with nothing
else), nor a raw handle a caller could use to inspect the pipe from outside it: `MasterPty::
try_clone_reader` erases to `Box<dyn Read + Send>`, and downcasting the master to the concrete
`ConPtyMasterPty` (via the `Downcast` bound `impl_downcast!(MasterPty)` gives every implementor)
does not help either, since its only field is private with no additional inherent methods. These
paths are `#[cfg(windows)]`, never `#[cfg(not(unix))]`, so an unsupported non-unix target fails to
compile instead of silently inheriting Windows semantics. How this platform still gets a real exit
ordering contract despite the above: see the amendment below.

**Amended 2026-09-22 (#504):** The output channel is now `futures::channel::mpsc` rather than
`std::sync::mpsc::sync_channel`, so a GPUI task can `.await` it instead of `crates/jerry-app`
polling it on a timer — the reader thread drives its `Sender` with
`futures::executor::block_on(tx.send(chunk))`, which blocks exactly like the old `sync_channel`
did once the bounded channel fills, preserving this entry's backpressure contract unchanged.
Process exit is now an item on that same channel (`PtyOutput::Bytes(Vec<u8>) | Exited(ExitStatus)`),
produced by a dedicated thread (`run_wait_loop`) that owns the `portable_pty::Child` handle for the
rest of the session and makes the one blocking `Child::wait()` call — a real, platform-uniform exit
signal (on Windows, `portable-pty`'s own `WaitForSingleObject` on the process handle) that replaced
the Windows-only independent `try_wait` poll this entry originally called for and, on unix, the
`eof_poll_decision` retry dance that used to bound the race between observing pty EOF and the child
actually being reaped. `PtySession::try_wait` stays (now a non-blocking peek at whether that thread
has finished, via `JoinHandle::is_finished`), but nothing in `crates/jerry-app` calls it anymore.
`PtySession::pid`/`killer` are cached/cloned at spawn time, before `child` moves to that thread, so
`process_id()`/`kill()` don't need it back.

**Exit ordering:** an initial version had `run_wait_loop` send `Exited` on the output channel
directly on every platform, racing the reader thread's own trailing `Bytes` sends - both threads
held a `Sender` with no ordering between them, so a chunk read just before real exit could arrive
after `Exited`. Observed for real on Linux and macOS CI: a 200,000-line counting test lost the
last few hundred lines, and a grid built by draining "until `Exited`" (the natural way to read a
terminal stream) missed content that had, in fact, already been written. The contract now held on
every target - `Exited` is always safe to treat as the stream's terminal item - is a hard guarantee
on unix and a strong, real-world-verified heuristic on Windows, achieved differently because the
platforms offer genuinely different tools, not because one target got less engineering effort.

*Unix* (a hard guarantee): the reader thread has sole ownership of `output_tx` - `run_wait_loop` no
longer sends on it at all. Instead it hands its `ExitStatus` to the reader over a one-shot
`std::sync::mpsc` channel and wakes it via a second self-pipe (`exit_read`/`exit_write`, alongside
the existing shutdown one). On that wake the reader (`drain_final_output`) does a final bounded
drain - `filedescriptor::poll` with a zero timeout, a genuine non-blocking readiness check, not a
real-EOF wait - so an orphaned descendant still holding the pty slave open cannot make this hang:
the drain stops the moment nothing is immediately available, then sends `Exited`. `crates/jerry-app`'s
`TerminalPane` and this crate's own test drains treat `Exited` as terminal (stopping at the first
one) rather than draining past it.

A quiet child (nothing left holding the slave open) routinely reaches real pty EOF/hangup on its
own *before* `run_wait_loop`'s `Child::wait()` returns and wakes `exit_read` - a fast `wait()` for
the parent still has to wait out process-table bookkeeping the kernel already did at child exit,
including closing the child's fds. Linux reports that as an `EIO` read error; macOS - whose
`filedescriptor::poll` is `select`-backed rather than real `poll(2)`, and so reports the fd as
plain read-ready instead of `POLLHUP` - as an `Ok(0)` read. Earlier code treated either as the
stream ending and returned from the reader thread immediately, which is exactly backwards: nothing
had signalled `exit_read` yet, so `Exited` was simply never sent and the guarantee above did not
hold on either platform. The reader now treats `EIO`/`Ok(0)` as "master done, not thread done": it
stops polling/reading the master fd (avoiding a busy spin against Linux's still-set `POLLHUP`) and
blocks on `[shutdown_read, exit_read]` alone until one fires, then proceeds exactly as above.

*Windows* (a strong heuristic, not a hard guarantee, and documented as such): `PeekNamedPipe`
against a real handle to the ConPTY output pipe was considered and is not reachable at all (see the
Windows paragraph above for what was actually checked) - but even a real handle would only have
bought a heuristic here, not a hard guarantee, because ConPTY translates the child's console output
to VT on its own internal thread and writes to the pipe asynchronously; a race against that
translation pipeline exists inside ConPTY itself, outside anything jerry-pty could observe from
outside it. The reader-idle quiet window is therefore the strongest observable signal actually
available. The reader thread flips a shared `AtomicBool` (`delivering`) to `true` from the moment
`read` returns a chunk until it has reached `output_tx`, and back to `false` before every blocking
`read` call. Once `Child::wait` returns, `run_wait_loop` sends `Exited` only after `delivering` has
read `false` continuously for `WINDOWS_EXIT_QUIET_WINDOW` (any `true` reading restarts the window,
since a chunk was in flight), backstopped by `WINDOWS_EXIT_GRACE_DEADLINE` so a descendant that
never goes quiet cannot stall exit reporting forever. Both constants are the Windows *post-exit*
grace only, never consulted in the steady state. Widened once already, from an initial 25ms/500ms
to 150ms/2000ms, after real runs on real Windows hardware under heavy concurrent load (many pty
tests, and other processes on the same machine, competing for the same cores) showed the tighter
values flaking - per this project's own testing philosophy, flakes get fixed by widening the real
margin, not by weakening what the test asserts. `exited_is_always_the_last_item_after_every_byte_
the_child_wrote` (50 iterations, a real `cmd /c echo` child on Windows, real `sh`/`printf` on unix)
pins this for both platforms with the same assertions.

One more consequence, GPUI-specific: `crates/jerry-app`'s tests that spawn a real `TerminalPane`
must call `cx.background_executor().allow_parking()` first (done once, centrally, in
`TerminalPane::new` and gated `#[cfg(test)]`) — `jerry-pty`'s reader/wait threads now wake the
pane's output task through a real cross-thread waker, and GPUI's test scheduler treats any wake
from a thread other than the test's own as non-deterministic and fails the test at teardown
unless this has been called. A real shell process (`cmd.exe` on Windows in particular, which sets
its own OSC 0 window title as part of its ConPTY startup handshake) can also now report state to
the grid before a test's own `run_until_parked` returns, where the old polling design accidentally
never drained it at all; tests asserting on a pane's "hasn't reported anything yet" state need to
clear it explicitly (`TerminalPane::reset_grid_for_test`) rather than assume it.

## 9. `crates/test-support` is a real crate, not a feature-gated one

**Status:** Accepted.

**Context:** Test setup had no shared home, so it was copy-pasted instead: `fn git(dir, args)`
appeared ~30 times across `jerry-git` and `jerry-app`, alongside 1,223 separate tempdir setups and 303
wall-clock waits. The obvious single crate to fix that has a trap in it — `crates/jerry-app`'s fixtures
need `gpui` (a test window, `VisualTestContext`), and `jerry-git`/`jerry-pty`/`jerry-lsp` must be able
to dev-depend on the same crate without acquiring `gpui` (§1).

A Cargo feature (`test-support = { features = ["gpui"] }` for `jerry-app` only) looks like it solves
this and does not: features unify across a workspace build, so one crate enabling `gpui` enables it
for every other crate resolving the same dependency. The core crates' dev graph would silently
regain `gpui` — exactly the outcome §1 exists to prevent, and one no `Cargo.toml` review would
catch.

**Decision:** Two homes, split by dependency rather than by feature. `crates/test-support` is
`gpui`-free and depends only on `tempfile`; anything needing `gpui` lives in
`crates/jerry-app/src/test_support.rs`, inside the one crate already allowed to have it.

**Consequences:** `cargo tree -e normal,dev,build -i gpui` reaching only `crates/jerry-app` is a
checkable invariant, not a convention. A helper that "just needs a `TestAppContext`" is not added
to `crates/test-support` under any flag — it goes in `crates/jerry-app` or it is restructured to take
plain data. The policy those fixtures serve is [`docs/testing.md`](../testing.md).

## 10. Every non-PTY child process is constructed through `jerry_pty::new_std_command`

**Status:** Accepted.

**Context:** The release binary is a GUI-subsystem process on Windows (`windows_subsystem =
"windows"` in `crates/jerry-app/src/main.rs`, adopted from Zed in #451 so no console opens behind the
window). On Windows, a console-subsystem child — `git.exe`, an npm `.cmd` shim, `cmd /c start` —
spawned from a consoleless parent allocates its own *visible* console window unless the spawn
passes `CREATE_NO_WINDOW`. Jerry spawns git continuously from launch (status poll, worktree
watch, Changes refresh), so the missing flag showed up as an endless storm of console popups
(#465). Zed pairs the same attribute with construction-time wrappers
(`util::command`/`gpui_util::new_std_command` upstream) plus a clippy ban on bare constructors;
#451 copied the attribute without the wrapper.

**Decision:** One constructor, `jerry_pty::new_std_command`, sets `CREATE_NO_WINDOW` on Windows
and is the identity elsewhere; every production `std::process::Command` in the workspace is built
through it. It lives in `jerry-pty` because that crate already owns "how children are spawned on
this OS" (`resolve_on_path`), and a one-function helper does not earn its own crate. `jerry-git`
takes a dependency on `jerry-pty` for it — the constructor must exist in exactly one place.
Enforced by `clippy.toml`'s `disallowed-methods` on `std::process::Command::new`; test modules
are exempt (each crate root's `cfg_attr(test, allow(clippy::disallowed_methods))`,
`test-support`'s crate-level allow) because the test runner owns a console its children inherit.

**Consequences:** PTY children are explicitly out of scope — `portable-pty`'s ConPTY spawn
already attaches them to a headless pseudo console via `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`, and
`CREATE_NO_WINDOW` is neither needed nor passable there. Anything that spawns without
`std::process::Command` (direct `CreateProcessW` FFI, a future async runtime's command type)
must apply the same flag at its own call site; none exists today.

## 11. A process-wide kill-on-close job object backstops child cleanup on Windows

**Status:** Accepted.

**Context:** Every cleanup path for spawned children — `PtySession::drop`/`kill`/`shutdown`'s
`taskkill /T` tree kills, `HookFiles::drop`'s temp-dir removal — is code running *inside* the
Jerry process. A force-killed (Task Manager "End task", `taskkill /F` without `/T`), crashed, or
aborted Jerry runs none of it, and unlike a unix process group, a Windows child is simply not
affected by its parent dying. In practice that leaked ~180 orphaned `claude.exe` agents per day,
which the user's own settings hooks amplified into ~10,000 processes (#482). `portable-pty`
creates no job object of its own.

**Decision:** At the top of `main()`, before anything can spawn, `crates/jerry-app`'s `job_object`
module creates one unnamed job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE |
JOB_OBJECT_LIMIT_BREAKAWAY_OK` and assigns *this process* to it. Children join a member's job
automatically at `CreateProcess` time, so every spawn — agent PTYs through ConPTY, every
`new_std_command` child, LSP servers, future call sites — is covered with no per-site plumbing.
The job handle is deliberately never closed: the kernel closes it when the process terminates,
by any means, and then kills every remaining member. Setup failure is logged and non-fatal
(behavior degrades to exactly the destructor-only world this replaces).

It lives in `crates/jerry-app`, not `jerry-pty`, for two reasons: the job is app-lifecycle policy, not
per-session PTY mechanics, and CLAUDE.md pins the core crates as `unsafe`-free — `crates/jerry-app`
already carries the sanctioned Win32 FFI sites (`hooks/settings_file.rs`,
`status_bar/process_stats/windows.rs`) and the `windows-sys` dependency.

**Consequences:** The in-session kill paths (`taskkill /T` and friends) stay: the job only fires
when the whole process dies, while sessions are killed and discarded continuously during normal
use. A child that must *outlive* Jerry has to break away explicitly — the updater's relaunch is
the one such child, spawned with `CREATE_BREAKAWAY_FROM_JOB` (permitted by `BREAKAWAY_OK`), with
a logged no-breakaway retry for the corner where an outer job forbids it. If job creation or
self-assignment fails, orphans are again possible; the startup sweep of `jerry-hooks-*`
directories (`hooks/settings_file.rs`) remains the independent cleanup for what a dead instance
leaves on disk.

## 12. A hookless agent CLI with no config-path flag gets a merged, opt-in entry in its own global
config file, never a file inside the worktree

**Status:** Accepted.

**Context:** GitHub issue #239 phase 2 gave Claude Code a real status side-channel by generating a
whole `--settings <path>` file Jerry owns outright and passing it on the CLI - zero footprint,
because nothing is shared and nothing is written unless that exact flag is present. `cursor-agent`
(issue #479) has real, working hooks in the same spawn mode Jerry already uses, but no equivalent
flag or environment variable to point it at an alternate config: hooks load from exactly four
fixed locations, and the only one Jerry can reach at all is the user-level
`~/.cursor/hooks.json` - a file the user owns and other tools may already be managing entries in.
`<worktree>/.cursor/hooks.json` was considered and rejected: it would appear in Jerry's own
Changes pane and review diff, and the agent could commit it - dirtying the very surface the diff
is supposed to review.

**Decision:** For an agent CLI shaped like this - real hooks, no config-path override, config
loaded from one global, shared, `.json`-with-an-array-of-`{command, timeout}`-entries file - Jerry
does a real read-modify-write merge (`crates/jerry-app/src/hooks/cursor_hooks_file.rs`) rather than
generating the file outright: unparseable JSON aborts the whole operation untouched, every
unrelated key and entry survives byte-for-byte, and Jerry's own entries are identified by a
forwarder script path substring rather than a marker field (the entry shape has no room for one).
The forwarder script itself moves out of the per-launch, `Drop`-deleted temp directory
`crates/jerry-app/src/hooks/settings_file.rs` uses for Claude into a stable, version-stamped path under
Jerry's own config dir, because `~/.cursor/hooks.json` outlives any single Jerry process. Because
this genuinely writes into a file the user owns - unlike Claude's entirely Jerry-owned, per-launch
`--settings` file - it is gated behind an explicit, default-off setting
(`Settings.agents.cursor_hooks_enabled`) reconciled (installed or removed) on every launch and
immediately on toggle, rather than default-on the way a similar integration in another Cursor
client ships it.

**Consequences:** The forwarder itself stays inert without Jerry's own `JERRY_HOOK_PORT`/
`JERRY_HOOK_TOKEN`/`JERRY_AGENT_ID` environment, so a `cursor-agent` session started outside Jerry
is unaffected by the entry's mere presence - the opt-in setting gates whether Jerry *writes* the
entry, not whether a stray entry could ever do anything on its own. The next agent CLI that is
hookless-by-default in this same shape (a real hook mechanism, no config-path flag, one shared
global config file) should read this entry and `cursor_hooks_file.rs` before inventing a new
merge strategy.

## 13. Every crate is named `jerry-<role>`

**Status:** Accepted (2026-09-22, issue #493).

**Context:** The workspace carried two naming families: role-only `app` and the `-core` adapters
(`wt-core`, `pty-core`, `lsp-core`). The UI-optional architecture plan (tracking epic #492) adds
`jerry-core`, `jerry-host`, `jerry-cli` and `jerry-ui`; with adapters still ending in `-core`,
`jerry-core` would read as a fourth adapter rather than the application-layer contract crate. `wt`
was also opaque to anyone outside the project.

**Decision:** One rule, no exceptions: every crate is `jerry-<role>`.

| Before | After | Role |
|---|---|---|
| `wt-core` | `jerry-git` | git adapter (`gix` reads, argv `git` writes) |
| `pty-core` | `jerry-pty` | PTY spawning adapter |
| `lsp-core` | `jerry-lsp` | language-server adapter |
| `app` | `jerry-app` | GPUI shell; bin target `jerry-app` |

`test-support` is unchanged: dev-only, never shipped. Rejected: Zed-style bare role names (`ui`,
`cli`, `host`), which keep two families alive and grep-collide with Zed's own crates during vendor
reading; and `jerryd`, a Unix daemon-ism on a project whose second platform is Windows.

**Consequences:** The bundle scripts still install the GUI binary under the product name `jerry`
until `jerry-cli` (#499) claims that name and the GUI ships as `jerry-app`. The conventions ratchet
greps `jerry_git::`/`jerry_pty::`/`jerry_lsp::` and its baseline counts are unchanged, since the
rename moves no call. Older entries in this file were rewritten to the new names in the same PR;
`CHANGELOG.md` keeps the names each release shipped with, and the table above is the map.

## 14. Windows spike: a session host can outlive the app (GitHub issue #494)

**Status:** Accepted - all four points pass on real Windows 11 hardware. The spike branch itself
(`spike/494-windows-host-survival`) never merged and issue #494 stayed open; this entry exists so
the later citations of "§14" throughout `jerry-pty`, `jerry-host` and decisions.md §24 resolve to
something real rather than a dangling reference. Condensed from the spike's own findings - see
`crates/jerry-app/src/windows_host_survival_spike.rs` on that branch for the full multi-process
test harness, not reproduced here.

**Context:** Stage 3 of the `jerry-core`/`jerry-cli`/`jerry-host` overhaul needs `jerry-host` to
survive the app that spawned it - a restart, a crash, an update. Decision §11's kill-on-close job
kills *everything* when Jerry dies, by design, the opposite of what a surviving host needs, so
before any of stage 3 was built this had to be proven possible on Windows at all.

**Decision - the four points, and what exactly passed:**

1. **PASS** - a process inside a kill-on-close job survives its own exit's job-close by spawning a
   child with `CREATE_BREAKAWAY_FROM_JOB`. The exact shape: `creation_flags` *replaces* rather than
   ORs into whatever a prior call set, so every flag that must survive has to be repeated in the
   same call - `CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP` together
   (`jerry_pty::new_detached_command`, §24). The parent then returns, its job's last handle closes
   with it, and the child - polled by a real Win32 liveness check
   (`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, ..)` +
   `WaitForSingleObject(handle, 0)`) - is still running afterward.
2. **PASS** - a job that forbids breakaway is detected and reported as a typed error, never a
   silent in-job fallback spawn. Detection is a real, second Win32 read distinct from interpreting
   the failed spawn's OS error code: `IsProcessInJob(GetCurrentProcess(), null, &mut in_any_job)`
   to confirm the process is jobbed at all, then `QueryInformationJobObject(null,
   JobObjectExtendedLimitInformation, &mut info, ..)` with a **null job handle**, which answers for
   the calling process's own job with no handle to it needed -
   `info.BasicLimitInformation.LimitFlags & JOB_OBJECT_LIMIT_BREAKAWAY_OK == 0` is the forbidding
   case (`jerry_app::job_object::breakaway_is_forbidden_for_current_process`, §24).
3. **PASS** - the surviving child gives its *own* descendants a fresh kill-on-close job,
   independent of whatever job (if any) contains it - the same job-object creation/assignment
   pair decision §11 already uses, reused unmodified and now proven to also work for a process
   that arrived at its current state via breakaway rather than at cold start.
4. **PASS** - the host spawns a ConPTY session through `jerry_pty::spawn` (unmodified), the
   original parent has already exited, and a third process keeps reading live output and can
   resize it. One real Windows-specific snag, not a claim failure: ConPTY's own startup Device
   Status Report query (`\x1b[6n`) withholds further output until answered, exactly as
   `jerry_pty::pty_session_tests` already documents for its own Windows tests.

**Consequences:** Stage 3 proceeds without a Windows-specific fallback design - the same
`CREATE_BREAKAWAY_FROM_JOB` + self-owned job + `jerry_pty::spawn` shape this spike proved is real
`jerry-host` architecture (§24), not a spike-only trick. A process that escaped a job via
breakaway is not reachable by anything in its spawner's own process (no `Drop`, no `ChildGuard`,
no tree-kill) - real `jerry-host` needs its own idle/linger self-check for exactly this reason
(§24's `Host::run_lifecycle`), not as an afterthought.

## 15. `jerry-core` is a contract crate: JSON-RPC 2.0 on the wire, `Locality` on every action, no threads

**Status:** Accepted (2026-09-22, issue #495; decisions Q3, Q4 and Q10 of the UI-optional plan).

**Context:** The application layer described in §2 had no crate. The plan's first pass drew a
`jerry-core` that also owned the socket listener and a "tight pump", and a bespoke
length-prefixed frame with an `id` field reserved for later server push. Three things needed
settling before the first client existed: what travels on the wire, which process may execute a
given action, and where I/O lives.

**Decision:**

- **JSON-RPC 2.0** over a big-endian `u32` length-prefixed frame. Requests carry an `id`,
  notifications do not, so server push and fire-and-forget hook events exist from day one. Methods
  are namespaced `command/<name>`, `validate/<name>`, `query/<name>`, `event/<name>`, plus `hook`.
  A Command's own outcome is always a `result` carrying a `Report` (`ok` / `denied` / `error`);
  JSON-RPC errors are reserved for transport and policy failures (unknown method, bad params,
  unsupported version, forbidden by `Invocability`, cwd outside the agent's worktree, needs a
  host). `hook` is a request with a short deadline rather than a notification, so a future `Stop`
  steering reply needs no framing change. Rejected: gRPC/tonic (tokio and protobuf for a local
  socket), Cap'n Proto (fd passing we do not need yet), MessagePack-RPC (not inspectable from a
  shell), tarpc (ties the wire to Rust types, and MCP would still need JSON-RPC beside it).
- **`Locality { Git, Session }`** is a required method on both traits, so adding an action forces the
  classification (unlike `Invocability`, which a Query may leave at its `Allowed` default). Git-locality Queries run locally in any process.
  Session-locality anything reaches the host. Git-locality Commands go to the host when one exists
  and run locally only in standalone mode, because the host owns the worktree-to-session coupling
  (`DiscardWorktree` must kill the agent living there). The trust boundary is the user, not the
  process: the socket is reachable only by the user's own uid, and an agent's identity is what
  its environment says. `Invocability` and cwd confinement keep an agent from acting outside its
  lane by accident; they are guardrails, not a sandbox against a process that lies about itself.
- **`jerry-core` owns no threads.** Types, codec, stable error codes, the registry and a blocking
  client connect, plus the Git-locality implementations. The listener, dispatch task and session
  table are `jerry-host`'s. Every method carries the same envelope, `{cwd, agent?, params}`, so a host classifies and
  confines every caller the same way. The wire contract is verified mechanically: one JSON
  fixture per catalogued example under `crates/jerry-core/fixtures/`, compared and round-tripped
  by a table-driven test, and an exhaustive match that gives every `jerry-git` failure a stable
  kebab-case code. The client refuses a descriptor of another protocol version before connecting,
  bounds every call as a whole, and poisons itself after a mid-frame failure rather than reading
  stale bytes on the next call.

**Consequences:** The GUI consumes `Report`s only (plan decision Q1), so it re-derives detail
through local Queries and nothing in it changes when the host becomes a process. The codec is
duplicated from `jerry-lsp`'s JSON-RPC client rather than shared, since `jerry-lsp` cannot depend
on this crate without a cycle. Which executor a Git-locality Command reaches is the transport
choice `jerry-cli` makes (#499), not something this crate decides. `jerry-git` gained `worktree_root` so a `Ctx` can be built from any
directory inside a worktree.

## 16. Tasks wake on channels; a timer is only for time itself

**Status:** Accepted (2026-09-22, issue #496; decision Q9 of the UI-optional plan).

**Context:** Every source of asynchronous data in the app was consumed by a timer loop: the
terminal pane polled a `std` channel every 8 ms, the rail drained hook events every 3 s, file
watchers set an atomic flag that a 300 ms tick read back, LSP diagnostics were polled at 250 ms.
The pattern came from the core crates handing out `std::sync::mpsc` channels and flags, which
have no async waker, so every consumer had to wake itself. The first pass of the control-plane
design added a fourth timer, a "tight pump" for Commands, to work around the 3 s hook drain.

**Decision:** A task wakes when its data arrives. Producers hand out awaitable channels
(`futures::channel::{mpsc, oneshot}`, sendable from a plain thread); consumers spawn one task
per source and loop on `receiver.next().await`; GPUI's executor parks the task until then. The
host's dispatch thread sleeps on its job channel, the app's client awaits a oneshot per call, and
hook events are pushed as `event/*` notifications to every subscriber, the app's own client
included once `jerry hook` (#500) sends them. A timer remains only
where time is the semantics: caret blink, debounce, staleness, backoff, and a CPU sample, which is
a delta by definition. Migrating the pre-existing loops is out of scope; new code and every seam
the UI-optional plan touches follow the rule, `jerry-pty`'s output channel first (#504).

**Consequences:** No per-request latency floor, no idle wakeups, and the same shape whether the
producer is a thread in this process or, at stage 3, a socket. The one cost is that a producer
must be given its channel rather than polled, which is why `jerry-core` exposes no threads and
`jerry-host` owns the only ones.

**Amended 2026-09-23 (issue #506): a `block_on` over an executor-driven future deadlocks GPUI's
single-threaded test scheduler; over an independent thread's reply, it does not.** Found the hard
way: a first cut of `SocketSessionAdapter`'s `shutdown` closure called
`futures::executor::block_on(LocalClient::request(..))` from inside a `cx.background_executor().
spawn` task, with no `.await` of its own inside that closure. `TestScheduler`
(`crates/scheduler/src/test_scheduler.rs` in the real `gpui` checkout) is single-threaded by
design and cooperative - `run_until_parked` is a `while tick(false) {}` loop on the one thread that
created it, and `LocalClient::request`'s own future can only make progress if the in-process
dispatch-loop task is itself polled forward by that same loop. Blocking that one thread inside
`block_on`, waiting on a future that only *that thread* can drive, is an unconditional deadlock -
confirmed empirically (a scratch reproduction that spawned the dispatch-loop task, forced it to
suspend on its own channel with a real `run_until_parked`, then spawned a `block_on`-inside-
`background_spawn` blocker: this hangs and is only killed by nextest's own slow-timeout; without
the intervening `run_until_parked` the two tasks happen to interleave in a way that hides the bug).
The rule is structural, not "which client happens to be in use": **nothing reachable from a
GPUI-scheduled task may `block_on` a future whose progress depends on GPUI's own executor turning
another task forward - regardless of whether that's `jerry_host::LocalClient` in a test or some
future in-process stand-in.** The fix is to never let such a `block_on` exist at all: `RemoteRepoHost`
(`crates/jerry-app/src/repo_host.rs`, decisions.md §24's own amendment) gives every out-of-process
dispatch a dedicated OS worker thread owning a plain `jerry_core::client::Client`, so the only
`block_on`-shaped code left is a `std::sync::mpsc::Receiver::recv` waiting on a reply from a thread
whose progress is completely independent of GPUI's scheduler - safe from any context, proven by
`repo_host::remote_repo_host_tests::dispatch_is_safe_from_inside_a_background_executor_task_under_
the_test_scheduler`. A `#[cfg(test)]` in-process `Connection::InProcess` still exists (§24), but
its own `RepoHost::dispatch` reaches it through `cx.spawn(async move |_, _| client.request(..)
.await)` - a real `.await`, not a `block_on` - never from inside a `background_spawn` task; nothing
in this codebase should ever pair `block_on` with `LocalClient` again.

## 17. `jerry` is the CLI; the GUI binary is `jerry-app`

**Status:** Accepted (2026-09-22, issue #499; decisions Q6 and Q8 of the UI-optional plan).

**Context:** Until now the GUI shipped under the product name `jerry` in every bundle. The command
agents and humans type, `jerry wt new --agent`, `jerry status`, must own that name, and cargo
refuses two `jerry` bin targets in one workspace anyway.

**Decision:** `crates/jerry-cli` builds the `jerry` binary; the GUI's bin target and bundled
executable are `jerry-app`, displayed as "Jerry" (the `Code.exe` / `code` pattern). Every bundle
ships both side by side: `Contents/MacOS/{jerry-app,jerry}` on macOS, `bin/{jerry-app,jerry}` on
Linux, and `Jerry.exe` next to `bin\jerry.exe` on Windows, where a case-insensitive filesystem
cannot hold `Jerry.exe` and `jerry.exe` in one directory. The CLI is bi-mode: it connects to the
Jerry serving the repository when one is published, and runs Git-locality requests itself
otherwise, choosing by `JERRY_HOST_SOCKET`, then `--instance`, then the registry. Its exit codes
are a contract, two codes distinct only when the caller must do two different things: 0 done,
1 done but action required, 2 usage, 3 refused (nothing happened), 4 no instance reachable when
one was required, 5 execution failed. `--json` is explicit, never inferred from the terminal.

**Consequences:** `jerry-app` finds `jerry` for hook and skill injection as a sibling, then under
`bin/`, then on `PATH`, and otherwise shows a visible error state rather than injecting nothing
silently; that locator arrives with `jerry hook` (#500), its first caller. Until then the CLI ships
in the bundles unused by the app.

## 18. The merge pilot: index mutations are Commands, disk edits and reads stay local

**Status:** Accepted (2026-09-22, issue #498; decisions Q1 and Q11 of the UI-optional plan).

**Context:** Merge was the first flow migrated to the Command model because it is the one with
a continuation and in-memory state. The question it settled is where the line runs between
"goes through the host" and "runs in the app".

**Decision:** Every mutation of git's own state is a Command dispatched through the host:
`MergeAttempt`, `MergeBranchIntoCurrent`, `MergeComplete`, `MergeAbort`, and `StageResolved`.
Each carries a `validate` that answers "could this run" without running it (`merge_preflight`
in `jerry-git` is `attempt_merge` minus the merge), and every one is `Denied` to agents, since
git already gives an agent a merge. Resolving a hunk is a disk edit under write-through (§ the
#497 change) and stays a local write; only staging the finished file touches the index, so only
staging is a Command. The GUI consumes the wire `Report`, which names paths and hunk counts, and
re-reads the conflicted files from the base worktree itself: contents never travel, and a
socket client in stage 3 does exactly what the in-process client does today. Git reads the
resolver needs at render latency (`classify_conflicted_file`, `load_conflicted_file`,
`merge_head_exists`, `find_in_progress_merge`) stay local per §15's `Locality` rule.

**Consequences:** `crates/jerry-app/src/merge/flow.rs` no longer calls `attempt_merge`,
`complete_merge`, `abort_merge` or `stage_conflict_resolution`; the plan's "no `jerry_git::`
call remains in `merge/`" is therefore met for mutations and deliberately not for reads. Every
test app runs an unpublished in-process host on the deterministic test executor, so the existing
merge tests drive the real dispatch path without a socket. `MergeFlowState` keeps its shape:
its `Conflicted` variant already isolates the cursor, and the outcome-versus-error split the
plan asked for was already there.

## 19. `jerry hook` replaces the curl forwarder; hook facts arrive as host notifications

**Status:** Accepted (2026-09-22, issue #500; decision Q10 of the UI-optional plan).

**Context:** `hooks/server.rs` was a hand-rolled HTTP/1.1 listener on loopback, and
`hooks/settings_file.rs` generated a per-launch forwarder script (`sh` on Unix, PowerShell on
Windows) that curled `http://127.0.0.1:$JERRY_HOOK_PORT/hook?event=…` with a bearer token from
`JERRY_HOOK_TOKEN`. Both the port and the token were launch-specific, baked into the generated
`--settings` file's environment at spawn time - so an agent that outlived a Jerry restart kept
posting to a dead port and a token nobody would ever check again (issue #274's finding). §17
already gave every launch a real `jerry` binary to inject as a sibling of the running app; this
issue spends it on the hook side-channel, its first caller.

**Decision:** A generated hook entry now runs `<located jerry> hook <Event>` directly - no
forwarder script, no curl, no port, no token. `jerry hook`
(`crates/jerry-cli/src/lib.rs::hook`) reads stdin, parses it as JSON (falling back to
`{"raw": "<text>"}` so nothing already-broken payload is silently dropped), sends
`Request::Hook(HookEvent { event, payload })` through the same `Session` every other subcommand
uses (`JERRY_HOST_SOCKET` then registry discovery), and **always exits 0, within a bounded
time** - never blocking or failing the agent's own tool call. Three separate bounds make that
true rather than assumed: the stdin read is capped at `MAX_HOOK_PAYLOAD_BYTES` (1 MiB, mirroring
`crate::hooks::event::MAX_PAYLOAD_BYTES` in the app, well under `jerry_core::wire`'s 16 MiB
frame limit) via `Read::take`, silently truncating rather than erroring on an oversized payload;
the read itself runs on its own thread and is bounded by `HOOK_STDIN_DEADLINE` (a few seconds)
via a channel `recv_timeout` (`read_hook_stdin`), so a stdin pipe that never sends EOF cannot
hang the hook either - on expiry that thread is abandoned, not joined, since there is no
portable way to cancel one blocked in a `read` syscall; and once a payload is in hand, the RPC
itself is bounded by a short, dedicated `HOOK_CALL_TIMEOUT` (a few seconds) rather than the
interactive-command one. The injected environment shrinks to `JERRY_AGENT_ID` plus
`JERRY_HOST_SOCKET` (`crate::hooks::settings_file::{AGENT_ENV, SOCKET_ENV}`);
`crate::host::find_jerry_binary` (sibling of `current_exe`, then `bin/` next to it, then `PATH`)
locates the binary named in the generated command, and hook injection is withheld - not offered
with a broken command - when it can't be found, or while the session host is still starting (a
real, transient window every launch passes through once, not counted against the existing
"bring-up attempted once" gate in `hooks/flow.rs`).

On the receiving end, `jerry-host`'s dispatcher (already built by #495/#496) fans a `hook`
request out to every connected client as an `event/hook` notification
(`{agent, cwd, event, payload}`); `crate::hooks::HookRuntime::start` now takes the host's
`LocalClient` and a `Context<AdeApp>` and spawns one `cx.background_spawn` task
(`crate::hooks::spawn_consumer`) that loops `while let Some(message) = events.next().await`,
feeding each `event/hook` notification's `event`/`payload` through the unchanged
`event::parse` into the same `HookInbox`/`EditLog` pair `HookListener` used - moved, verbatim
pure logic, into the new `crate::hooks::inbox` module once `server.rs`'s TCP listener had
nothing left to own. `HookRuntime`'s public surface (`signal_for`, `text_for`, `session_id_for`,
`run_facts_for`, `drain_edits`, `forget`) is unchanged, so `flow.rs` and the rail's poll are
untouched. `cursor_hooks_file.rs`'s managed entries move the same way, but its own
own-entry-matching couldn't keep using a stable, Jerry-owned forwarder directory (there is no
forwarder to own a directory for): entries are now matched structurally, by
`jerry`/`jerry.exe` appearing right before ` hook ` in the quoted command
(`is_managed_entry`/`MANAGED_MARKERS`), which also means `remove_managed_entries` no longer
needs a locatable binary at all - turning the setting off must work even after the binary that
wrote an entry can no longer be found.

**Consequences:** No TCP listener anywhere in the workspace, and no token at rest, in
environment, or on a command line - `settings_file`'s own
`the_settings_file_carries_nothing_launch_specific_but_the_jerry_path_and_the_event_name` test
pins exactly that. `hooks/integration_tests.rs` and `provenance/integration_tests.rs` drive
`jerry_cli::run` in-process against a real `jerry_host::Host` on a temp socket rather than a
real `jerry` binary (none is available to a unit test) - and, discovered while writing them,
GPUI's deterministic `TestScheduler` panics ("your test is not deterministic") the instant a
genuinely independent OS thread wakes a `cx.background_spawn` task, which a real socket's
listener/dispatch threads eventually do once a subscriber exists. `jerry-host`'s own listener is
real OS threads by design (§15), so a `#[gpui::test]` can drive the real socket transport
(`jerry_cli::run` end to end) only as a plain, non-gpui test, and can drive the app's own
`HookRuntime` consumer only through the in-process `LocalClient` (`Call::agent` submitted
directly, exactly as a socket client's request would dispatch) - never both at once in the same
test. `gpui::TestAppContext::executor().allow_parking()` exists as an escape hatch for exactly
this (verified against `gpui`'s own `scheduler` crate), and is the fallback if a future test
genuinely needs both in one place, but every hook test here is expressible without it.

## 20. Rebase joins the pilot: `RebaseStart`/`RebaseContinue`/`RebaseSkip`/`RebaseAbort`/
`AmendHeadMessage`, `RebaseStatus`

**Status:** Accepted (2026-09-22, issue #501; decisions Q18 and Q19 of the UI-optional plan).

**Context:** Rebase was the second flow migrated to the Command model, after merge (§18). Unlike
merge, every mutation happens in one worktree — there is no second, base-worktree path to reason
about — but `RebaseStart` alone needs something no other Git-locality Command has needed yet: a
real, executable path (§7's `jerry` binary) to hand `jerry-git`, so `GIT_SEQUENCE_EDITOR`/
`GIT_EDITOR` have something real to spawn.

**Decision:** `RebaseStart { onto, plan }`, `RebaseContinue {}`, `RebaseSkip {}`, `RebaseAbort {}`,
and `AmendHeadMessage { message }` are Git-locality Commands, all `Invocability::Denied` to
agents (git already gives an agent a rebase); `RebaseStatus` is a Query mirroring
`jerry_git::rebase::rebase_status`. `RebaseStart::validate` splits a real `rebase_preflight` out
of `start_interactive_rebase` (refusing `rebase-already-in-progress` and `rebase-worktree-dirty`
without touching git, mirroring `merge_preflight`'s split from `attempt_merge`), and
`RebaseContinue`/`RebaseSkip`/`RebaseAbort`/
`AmendHeadMessage` all validate against `rebase_status`'s real on-disk state
(`rebase-not-in-progress`/`rebase-not-stopped`) rather than in-memory assumptions. Every outcome
and plan row is mirrored onto the wire (`RebaseOutcomeReport`, `RebasePlanEntryWire`,
`RebaseActionWire`, `StopReasonWire`) rather than deriving `Serialize` on `jerry-git`'s own types
directly — the same reasoning §18 already gives for `ConflictKind` — with `From` conversions both
ways so the app can keep building its in-memory plan and reading `RebasePhase` in terms of
`jerry_git::rebase`'s own plain-data types, converting only at the dispatch boundary.
`RebaseStart::execute` locates the `jerry` binary itself (`jerry_core::jerry_binary::locate`, a
sibling of `std::env::current_exe`, then `bin/jerry` next to it, then `jerry` one directory up —
no `PATH` fallback, unlike `crate::host::find_jerry_binary` in `jerry-app`, so this crate adds no
`jerry-pty` dependency), refusing `jerry-binary-not-found` rather than guessing when none exist.

`graph_view/rebase.rs` dispatches every mutation through `AdeApp::dispatch`, the same idiom
`merge/flow.rs` established: `start_rebase`/`skip_rebase` share `run_rebase_op` (dispatch, apply
the wire outcome); `continue_rebase` drives its own two-dispatch sequence (`AmendHeadMessage` when
a message-less reword was resolved, then `RebaseContinue`) since `run_rebase_op` is one dispatch
only; `abort_rebase` dispatches inline since its success path (leave the mode, reload the graph)
differs from the other three's (transition to `RebasePhase::Stopped`). The read-only calls
(`commits_to_rebase`, `commit_changed_files`, `commits_already_on_upstream`, `resolve_commit`)
stay local per §15's `Locality` rule, exactly as merge's resolver reads do.

**Consequences:** `CARGO_BIN_EXE_<name>` — needed to hand `start_interactive_rebase` a real,
spawnable `jerry` for a test — turned out reliable only for a `[[bin]]` of the *same* package
referenced from an integration test (`tests/*.rs`), not from a `--lib` unit test referencing it,
and not across a dev-dependency cycle back through `jerry-core`/`jerry-cli` either (both were
tried and both failed to see the environment variable at compile time, verified against this
issue's own build). `jerry-git` grew a minimal, same-package `[[bin]]` purely for this
(`jerry_git_test_editor`) and moved every test that needs a real spawn into
`tests/rebase_editor.rs`; what stayed in `--lib` is what never spawns anything (pure
classification, and sidecar-file tests that call `run_editor`/`run_sequence_editor` directly).
`jerry-core` tried the identical shape (`jerry_core_test_jerry`) first and hit a real, worse bug:
on Windows, cargo places a `[[bin]]`'s own unhashed `target/debug/deps/` copy under the *same
name* the top-level uplifted binary gets, so a helper copied to
`current_exe().parent().join("jerry.exe")` from inside an integration test (whose own executable
already lives in that same `deps/` directory) silently overwrote `jerry-cli`'s real
`deps/jerry.exe` — and the next build then uplifted that corruption to `target/debug/jerry.exe`,
breaking the real CLI for every other consumer, not just this test. `jerry-core`'s own `[[bin]]`
and its copy mechanism were deleted outright; `jerry_binary::locate` instead gained a third tier
(one directory up from `current_exe`, the layout an integration test's own executable — nested
one level under wherever a `[[bin]]` gets uplifted to — actually has), so `jerry-core`'s tests
find the real `jerry` `cargo build -p jerry-cli` produces directly, asserting that up front
(`assert_real_jerry_binary_available`, the same shape `jerry-app`'s own helper of that name
uses) rather than copying anything. This mechanism runs on every platform `jerry` does —
verified directly against this Windows checkout, both the quoted-path editor-hook spawn (§7) and
every one of `jerry-git`'s 15 real-rebase integration tests, previously Unix-only.
`jerry-cli`'s hidden `git-sequence-editor`/`git-editor` subcommands are the only part of this
issue's CLI surface; `jerry rebase` itself is out of scope; `jerry-git` promotes from a
`jerry-cli` dev-dependency to a normal one so those two subcommands can call
`jerry_git::rebase::run_sequence_editor`/`run_editor` directly - they are pure local sidecar-file
mechanics git itself spawns, not part of the Command/Query wire model, so they run before any
`Ctx`/transport is built and answer with git's own exit-code contract (0 accept, non-zero stop),
never `jerry-cli`'s own.

## 21. `WorktreeCreate` is `Invocability::Allowed`; the agent spawn is the host's reaction, not the
Command's

**Status:** Accepted (2026-09-22, issue #502; decision Q17 of the UI-optional plan).

**Context:** Every Command so far (§18, §20) is `Denied` to agents because git already gives an
agent an equivalent (a merge, a rebase). Worktree creation is the first genuine exception: git
gives an agent `git worktree add`, but nothing that also tells Jerry to supervise the result or
start a second agent in it - the thing an agent actually needs is the *supervision*, not the
worktree. This issue is also the first time anything reaches for the `jerry` CLI's own existence
from inside an agent's environment at all, which only matters once an agent can act on it.

**Decision:** `WorktreeCreate { branch, from, agent: Option<AgentSpec>, prompt }` is a
`Locality::Git`, `Invocability::Allowed` Command in `jerry-core`. `AgentSpec` (`Claude`/`Codex`/
`Cursor`, kebab-case on the wire) is an independent mirror of `jerry_app::work_surface::
agents::AgentKind`'s three kinds - jerry-core cannot depend on jerry-app (§1) - reunited by an
exhaustive `From<AgentSpec> for AgentKind` at the one dispatch boundary that needs it
(`work_surface::worktree_created`). `execute` places the new worktree as a sibling of the main
checkout, under `<main-dir-name>-worktrees/<branch, sanitized>` (never nested inside the main
worktree, which would show up there as an untracked directory in `git status`), through the
existing `jerry_git::add_worktree`, and reports `{ path }` only - never the agent, never the
prompt, which is `execute`'s whole point: the Command's own job ends at "the worktree now exists".
Preflight (`validate`) is deliberately narrower than the rest of the codebase's "collisions
surface as git's own error" convention (`jerry_git::checkout::create_branch_at`'s own docs): since
the target path is *this crate's own* deterministic function of `branch`, a real, cheap,
non-mutating existence check catches a same-branch retry before git ever runs, with a specific
`worktree-path-exists` code rather than a generic one. A colliding *branch name* (someone else's
worktree, not ours) still surfaces as git's own error, unchanged.

The spawn is `jerry-host`'s reaction to the outcome, not the Command's: `dispatch.rs` publishes
`event/worktree-created` (`{ path, agent, prompt, requested_by }`, `agent: null` for a plain
creation too) on the existing fanout after any successful `command/worktree-create`, mirroring
exactly how `event/hook` is already produced there. `jerry-app` subscribes once, in
`AdeApp::adopt_host` (a `Task` owned by `HostRuntime`, cancelled on drop - the channel-woken shape
§16 asks for, no timer): on the notification it refreshes the owning repository's worktree list
and calls the same `select_worktree_by_path` a rail click calls, then - when `agent` was given -
spawns it via `Agents::spawn`/the new `Agents::spawn_with_prompt` (a prompt becomes the CLI's own
leading positional argument, `claude`'s/`codex`'s real "start with this message" convention).

`jerry agents [--json]` is `AgentsQuery {}`, `Locality::Session`: local dispatch (`execute_locally`)
answers `NeedsHost` for it exactly like every other Session-locality request always has, and
`AgentsQuery::run` itself is an honest, never-actually-reached `Error` rather than a fake answer -
the real host special-cases `Request::Query(AppQuery::Agents(_))` in `dispatch.rs`, answering
directly from `AgentTable::list()` (extended from a bare `PathBuf` to `{ worktree, kind }`; `kind`
is whatever label the registering caller passes - `jerry-app`'s own `AgentKind::label()`, e.g.
`"Claude"` - never interpreted by `jerry-host`). `jerry wt new <branch> [--from <ref>] [--agent
<kind>] [prompt]` dispatches `WorktreeCreate`, printing the created path either way; standalone
with `--agent` still creates the worktree (Git-locality never needs a host) but warns on stderr
and exits `NO_INSTANCE` (4), since nothing is listening to spawn the agent it asked for.

Every agent this app spawns gets the directory holding its own `jerry` binary
(`jerry_core::jerry_binary::locate()`, sibling-of-self or `bin/` next to it - never a `PATH`
fallback, so this never reports a stranger's unrelated `jerry`) prepended to `PATH`
(`work_surface::agents::with_jerry_on_path`, the one place `ProcessKind::spec` builds an agent's
environment) - a `locate()` miss logs a warning and spawns anyway, never blocks. The `jerry` skill
(`crates/jerry-cli/skill/SKILL.md`, `include_str!`'d by both `jerry-cli` itself, for `jerry skill`,
and `jerry-app`'s `hooks/settings_file.rs` by file-system-relative path - never a crate dependency
edge, `jerry-cli` stays a leaf) is written as a minimal Claude Code plugin
(`.claude-plugin/plugin.json` + `SKILL.md` at its root - verified against a real `claude --help`
and <https://code.claude.com/docs/en/plugins.md>) into the same per-launch directory
`HookFiles::write_in` already owns, and passed as `claude --plugin-dir <dir>` alongside
`--settings`. `codex`/`cursor-agent` get PATH injection (kind-independent) but no skill injection:
neither binary's own `--help` on this machine documents a per-launch, no-user-setup equivalent to
`--plugin-dir`, and this issue does not guess one.

**Consequences:** `AgentTable::register`'s signature grew a third parameter (`kind: String`),
updating every call site in `jerry-app`, `jerry-host`, and `jerry-cli`'s own tests. The worktree
placement convention here (sibling, `<name>-worktrees/`) is this issue's own choice, not a
pre-existing one - `rail::repo::Repo::path`'s doc comment mentions a `~/.jerry/wt/<name>` layout
"per the revision doc", but no such document or convention exists yet anywhere in this codebase;
reconciling the two is left to whichever future issue actually introduces that layout. The
`#[gpui::test]` proving a real notification really causes a real second agent to spawn
(`work_surface::worktree_created::tests`) uses the in-process host, and the `external`-tier test
proving a real, autonomous `claude` really reaches for `jerry wt new --agent claude` on its own
uses a real threaded socket host with a plain `claude -p` subprocess
(`hooks::integration_tests::a_real_claude_session_uses_jerry_wt_new_and_the_real_host_hears_about_it`)
- never combined in one test, the same real-socket-or-in-process-consumer split §19's own
consequences section already documents for hooks.

## 22. `jerry mcp`: an MCP server generated from the `Request` catalogue, not hand-maintained

**Status:** Accepted (2026-09-22, issue #509).

**Context:** MCP is JSON-RPC 2.0, the same codec `jerry-core` already speaks (§15). The question
this issue settled was whether an MCP server's tool list is a second, hand-maintained catalogue
that can drift from `AppCommand`/`AppQuery`, or a real projection of the one catalogue that already
exists.

**Decision:** `jerry mcp` (`crates/jerry-cli/src/mcp.rs`) is a subcommand of the existing `jerry`
binary: a blocking read loop over newline-delimited JSON-RPC 2.0 on stdin, writing the same framing
to stdout - stdout is reserved for the protocol, every diagnostic goes to stderr, one request at a
time, no new async runtime. It reuses `jerry_core::wire::Message`/`RequestId`/`RpcError` rather than
a second codec; the one addition is `jerry_core::wire::{read_line_frame, write_line_frame}`, a
newline-delimited sibling of the length-prefixed frame the host socket uses, tested the same way.

- **Protocol version:** `2025-11-25`, the latest revision that still uses the classic `initialize`
  handshake (`initialize` then `notifications/initialized`, then `ping`/`tools/list`/`tools/call`).
  Verified directly against <https://modelcontextprotocol.io/specification>: the *current* spec
  revision is `2026-07-28`, but that revision replaced the handshake entirely with a stateless,
  per-request `_meta.protocolVersion` plus a mandatory `server/discover` call - the spec's own
  compatibility matrix calls everything before it "legacy" and documents that `2026-07-28` servers
  are expected to keep answering legacy `initialize` requests for exactly this reason. Real MCP
  clients, Claude Code included, still overwhelmingly speak the legacy handshake as of this
  writing; implementing `server/discover` instead would not interoperate with them today. This
  server supports exactly one protocol version and always answers `initialize` with it, which the
  legacy negotiation rule explicitly allows (a client that does not like it disconnects).
- **Tool list, generated, never hand-maintained:** one tool per `AppCommand`/`AppQuery` variant,
  built from `Request::examples()` (`jerry_core::mcp::all_tools`) so the tool catalogue cannot
  diverge from the wire catalogue - a new `Request` variant already has to appear in
  `Request::examples()` for the existing fixture test (§15) to pass, and now automatically becomes
  a tool too. **Tool name:** the wire method with `/` replaced by `_`
  (`command/merge-attempt` becomes `command_merge-attempt`) - a `.` separator was the brief's own
  first instinct and was rejected because MCP tool names must match `^[a-zA-Z0-9_-]{1,64}$`, which
  has no `.`. The substitution is bijective and has a test (`jerry_core::mcp::tool_name`/
  `method_for_tool_name`): a wire method's kind (`command`/`query`) and kebab-case name never
  contain `_` (`Method::parse`'s own `is_kebab` check), so the first `_` in a tool name is always
  the boundary the mapping put there. **Description:** a hand-written `&'static str` per variant in
  an exhaustive match (`AppCommand::description`/`AppQuery::description` in `request.rs`), mirroring
  each variant's own doc comment rather than extracting it mechanically - true rustdoc extraction at
  compile time needs a proc macro this issue did not add; a test asserts every description is
  non-empty. **Input schema:** `schemars = "1.0"` (the same line `vendor/zed/Cargo.toml` pins,
  verified against the real checkout), `#[derive(schemars::JsonSchema)]` added to every
  Command/Query input struct and the wire enums nested inside one (`AgentSpec`,
  `RebasePlanEntryWire`, `RebaseActionWire`) - never on outcome types, which no tool needs a schema
  for. `command.rs::schema_of::<T>()` wraps `SchemaGenerator::default().into_root_schema_for::<T>()`;
  schemars reads the same `#[serde(...)]` attributes already on these structs (tag, rename_all,
  default) with no extra `#[schemars(...)]` annotations needed anywhere in this pass.
- **Caller classification and `Invocability`:** `jerry mcp` classifies its caller exactly the way
  every other subcommand does - `JERRY_AGENT_ID` in the environment makes an agent, its absence a
  human (`crate::run`'s existing `Caller` derivation, unchanged). `tools/list` filters
  `jerry_core::all_tools()` through `jerry_core::permits(caller, tool.invocability)`, so an agent
  never even sees a tool it cannot call. A `tools/call` naming a tool the caller may not invoke is
  still checked again at call time (a client can call a tool it never listed) and answered with a
  normal `Ok` tool result, `isError: true`, `structuredContent: {"code": "forbidden", "reason":
  "..."}` - never a JSON-RPC error, so an MCP client's ordinary tool-result handling sees it rather
  than a transport-level failure. `forbidden` is this module's own kebab-case spelling of the
  socket's `rpc_code::FORBIDDEN`; the two are not the same wire shape (one is a JSON-RPC error code,
  the other a string in a tool result), so there was no existing kebab constant to reuse. Anything
  else a `tools/call` produces - `Report::Ok`, `Report::Denied` (a command's own `validate`
  refusing it, e.g. `merge-abort` with no merge in progress), `Report::Error` - becomes the tool
  result's `content[0].text` (the `Report` as compact JSON) plus `structuredContent` (the raw
  `Report`), `isError` set for anything but `Ok`. Each call dispatches through the same `Session`
  every other subcommand uses (host socket when one is running, local execution otherwise - §17).
  Malformed `tools/call` params (`name` missing, an unknown tool name, arguments that fail the
  target Command/Query's own `Deserialize`) are real JSON-RPC errors, same as an unknown top-level
  method (`METHOD_NOT_FOUND`/`INVALID_PARAMS`) - these are the client's own mistake, not an outcome
  of running anything.
- **Zero-setup registration for Claude Code:** the per-launch plugin directory `HookFiles::write_in`
  already writes (§21) gains a `.mcp.json` at its root (`crate::hooks::settings_file::
  mcp_manifest_json`) declaring a `jerry` server whose `command` is the located `jerry` binary and
  `args` is `["mcp"]`. Verified real and auto-loaded with no extra flag beyond the `--plugin-dir`
  Jerry already passes, at <https://code.claude.com/docs/en/mcp.md> and
  <https://code.claude.com/docs/en/plugins.md>. It carries no `env` field: `JERRY_AGENT_ID`/
  `JERRY_HOST_SOCKET` are meant to reach the spawned `jerry mcp` process exactly the way they
  already reach a spawned `jerry hook <event>` - inherited from the `claude` process's own
  environment, which `HookInjection::env` injects them into at spawn time, not written into this
  file at all. This is forced by the file's own shape as much as chosen: `.mcp.json` lives in the
  one plugin directory a whole Jerry launch shares (`HookFiles::write_in` is called once per
  launch, not once per agent spawn), so no single static value in it could ever name one specific
  agent among several sharing that directory. What is not independently verified: whether Claude
  Code's MCP stdio child-process spawn inherits the parent `claude` process's environment the same
  way its hook-command spawn does - both were checked against Claude Code's own docs, which
  describe `${CLAUDE_PLUGIN_ROOT}`-style interpolation for `.mcp.json`'s `env` field but do not
  state either way whether an omitted `env` field means full inheritance for an MCP child
  specifically. Ordinary child-process spawning inherits the parent's environment by default on
  every OS, and hooks already prove Claude Code's own command spawning does; this is believed to
  extend to MCP servers but is flagged here, not silently assumed solid. Other agent kinds get
  nothing: neither `codex` nor `cursor-agent`'s own `--help` on this machine documents a per-launch,
  no-user-setup MCP registration equivalent to `--plugin-dir`, mirroring §21's identical finding for
  skill injection.

**Consequences:** `jerry-core` gained a `schemars` dependency (cheap, no feature flag) and its
first module with no `jerry-git`/`gpui` involvement at all, `mcp.rs`. `jerry-cli/src/mcp.rs`'s own
test suite drives `crate::run(["mcp"], ...)` end to end through an in-memory pipe (initialize,
`ping`, `tools/list` for an agent vs. a human, a `tools/call` of `query_agents` against a real
in-process `jerry-host::Host`, a `Report::Denied` command and an `Invocability`-forbidden one both
becoming `isError: true` tool results, a malformed line answered without ending the loop, and EOF
exiting 0) - the same real-socket-vs-in-process-consumer boundary §19's consequences already
describe, since `jerry mcp` is a `jerry-cli` subcommand talking to a real socket, never the app's
own in-process `LocalClient`. `crates/jerry-cli/skill/SKILL.md` gained an "MCP" section so an
agent reading the skill knows the tools mirror the CLI one-for-one rather than discovering it by
trial and error.

## 23. `jerry-host` owns the session table and every PTY it spawns; the data plane is a separate
byte stream, never `Call`/`Report`

**Status:** Accepted (2026-09-23, issue #505; decisions Q2, Q12, Q13 of the UI-optional plan).
Part A (below) landed first; Part B (further down) closed most of "What did not move" in the same
issue. One piece - the hook store - is still open; see "What still has not moved".

**Context:** `AgentTable` (§21) already gave the host a table of *identities* - which worktree an
agent may act in - but the real `jerry_pty::PtySession` for every terminal tab, agent or plain
shell, still lived entirely inside `crates/jerry-app`'s `TerminalPane`, spawned with a direct
`jerry_pty::spawn` call. Two different things were both called "the session": the host's identity
record and the app's own process handle, with no single table naming a PTY the same way twice.
This issue was scoped to unify them - one `SessionId` per PTY, host-owned - while keeping the
byte stream itself off the JSON-RPC wire, since bytes are not a control-plane concern.

**Decision:**

- **One session table, in `jerry-host`** (`crate::session::SessionManager`): `SessionId` (a
  host-minted, wire-serializable newtype), `SessionKind::Pty` (the only kind today - an ACP kind
  is planned, out of scope here), `worktree`, `agent: Option<SessionAgentInfo { kind, agent_id }>`,
  `started_at`, `exit: Option<ExitStatusWire>`. `AgentTable` (§21) is now a thin view over this
  table (`SessionManager::agent_table`) rather than its own independent map: `register`/`forget`/
  `worktree_of`/`len`/`is_empty`/`list` keep their exact pre-existing signatures and behaviour
  (`jerry-app`'s own callers, and `dispatch.rs`'s `classify`/`confine`, are unchanged), but every
  entry they create or remove lives in the same `HashMap<SessionId, Entry>` a real spawn does -
  one source of truth, not two tables that can drift. An `AgentTable`-only registration (no
  process behind it, exactly `register`'s pre-existing contract) is a real, listed `SessionRecord`
  with `exit: None` forever; `SessionResize`/`SessionKill` against its id answer a real
  `session-not-owned` error rather than pretending to act on a process that was never spawned.
- **`SessionManager::spawn` owns the real `PtySession`.** It calls `jerry_pty::spawn` directly (the
  one new PTY-owning caller besides `jerry-app`), takes the session's own output stream once, and
  hands the caller back `(SessionId, Arc<SessionHandle>)`. `SessionHandle` is the data-plane
  adapter: `write_input` (delegates straight to `PtySession::write_input`) and `take_output`
  (hands out the relayed `futures::channel::mpsc::Receiver<PtyOutput>` exactly once - `None` on a
  second call). Neither travels through `Call`/`Report`; an in-process caller reaches a spawned
  session's handle directly via `Host::sessions().handle_for(id)` ("attach"), a plain Rust call,
  since a JSON envelope cannot carry a byte-stream receiver at all, in-process or otherwise. A
  **relay thread**, one per spawned session, is what makes this possible without the host needing
  to understand ANSI/grid state: it drains `PtySession::take_output`'s own stream on its own
  thread and forwards every item, in order, to the channel `SessionHandle` hands out - the same
  `Bytes*, Exited` shape and Exited-last ordering guarantee `jerry_pty::PtyOutput` itself documents
  (§8's amendment) is preserved by construction, since the relay only ever forwards, never
  reorders or drops. The moment it observes `Exited`, it records the exit on the table and
  publishes `event/session-exited { id, status }` on the same `Fanout` the rest of the host uses -
  before forwarding that same item downstream - so the control-plane notification and the
  data-plane byte stream's own terminal item are never out of step with each other. **Stated
  explicitly, per this issue's own scope note:** this is single-consumer - one relay, one
  `SessionHandle`, `take_output` gives out its receiver exactly once. A second attacher (two panes
  on one session) is not supported; it would need a real per-session fan-out in
  `SessionHandle::take_output` instead of a plain `Option::take`, which nothing here needed yet.
- **Control plane: `SessionSpawn`, `SessionResize`, `SessionKill`, `SessionsQuery`.** All
  `Locality::Session`; `SessionSpawn`/`SessionResize`/`SessionKill` are `Invocability::Denied` to
  agents (an agent already has a real pty of its own from whatever spawned it, and resizes/kills
  that one, not one Jerry is holding on someone else's behalf). Like `AgentsQuery` before them,
  none of the four ever actually reaches `Command::execute`/`Query::run`: `jerry-host`'s
  dispatcher special-cases all four, exactly as it already did for `AgentsQuery` and `Hook`,
  answering directly from `inner.sessions()`. `AgentsQuery` is unchanged on the wire and is now
  genuinely "implemented on top of" the same table `SessionsQuery` reads, for free, since
  `AgentTable` is that table's own view. `SessionSpawn`'s worktree is the caller's own `cwd` from
  the call envelope, never a field on the command - a caller cannot ask to spawn somewhere its own
  confinement would not otherwise reach.
- **A real shell, not a fake one, is what a Windows PTY consumer must answer.** Every jerry-host
  session test spawns `cmd /c`/`sh -c` for real. Doing so surfaced a real, pre-existing contract
  `crates/jerry-app/src/terminal/pane.rs` already had to honor and `jerry-pty`'s own tests already
  work around: on Windows, ConPTY withholds *all* child output until something answers its startup
  Device Status Report query (`ESC[6n`) with a cursor position report - a real consumer answers
  this from its VT parser (`jerry-app`'s pane, from `alacritty_terminal`'s grid); a bare test
  harness with no grid has to answer it by hand, exactly as `jerry-pty`'s own
  `answer_cursor_position_query` test helper does, or the session hangs forever, not just slowly.
  `jerry-host`'s own session tests needed the identical helper, since `SessionManager`'s test
  seam is likewise VT-blind by design.

**What moved (Part A):** the session table and the real `PtySession` for every session
`SessionManager` itself spawns; `AgentTable`'s storage (not its public shape).

**What Part A did not move, and why:**

- **`crates/jerry-app`'s `TerminalPane` still called `jerry_pty::spawn` directly** for every
  production tab (agent or shell) and still owned its own `PtySession`. Flipping every spawn call
  site to dispatch `SessionSpawn` through the host was not done, because `AdeApp::new` spawned the
  opened repository's first shell during `Self::new_with_settings`, and only called `Self::
  start_host` (which brings the host up asynchronously, on a background task) afterward - a
  `TerminalPane` could not reliably dispatch a Command through a host that provably did not exist
  yet at the moment it needed to spawn.
- **The hook store (`hooks/store.rs`) and the rest of `Agents` bookkeeping stayed in `jerry-app`.**
- Because of both of the above, `jerry-app` needed zero code changes for Part A: `AgentTable`'s
  public surface was identical, so every existing call site and test compiled and passed
  unmodified. The DoD's "pane tests against the in-process byte adapter" and "`AdeApp` reflects a
  session exit received as an event" were not met for the same reason - there was no real
  production consumer of `SessionHandle` yet to test honestly.

**Part B (same issue, later commits on the same branch): `TerminalPane` spawns through the host.**

- **The host's cheap, in-memory half starts synchronously, before any pane can spawn.**
  `HostRuntime::in_process()` (unpublished: no socket, no registry entry, reachable only from this
  process - what a test app already ran on) is now created inside `AdeApp::new_with_settings`
  itself, immediately after the struct literal, before `Self::load_worktrees`/`Self::
  spawn_initial_shell_for_opened_repo` can spawn the first shell. Only the slow half - registry
  publish and the real socket bind, both real filesystem/network I/O - stays deferred to
  `AdeApp::start_host`'s existing background task, which now calls the new `AdeApp::publish_host`
  (`HostRuntime::allocate_instance`/`HostRuntime::publish`, split the same way `HostRuntime::start`
  already bundled them) once that work finishes. A `TerminalPane` can now always dispatch
  `SessionSpawn` through a real, live host - the ordering problem above is closed by construction,
  not worked around.
- **`Agents::spawn_inner` dispatches `SessionSpawn` and attaches asynchronously; the tab appears
  synchronously.** The `Agent` (and its `TerminalPane`, unattached) is still pushed and made active
  in the same call that returns its `AgentId` - every caller's existing "spawn returns an id for a
  real, focusable tab" contract holds unchanged. A background `cx.spawn` task then dispatches
  `SessionSpawn`, resolves the returned `SessionId` to a real `SessionHandle` via `Host::
  sessions().handle_for`, and calls the new `TerminalPane::attach_session` - or `TerminalPane::
  mark_spawn_failed` on any real, honest failure (host unreachable, the dispatch itself erroring,
  a malformed outcome). `TerminalPane` no longer calls `jerry_pty::spawn`, or anything under
  `jerry_pty::`/`jerry_host::`, anywhere but through the new `pub(crate) trait SessionAdapter`
  (`id`, `write_input`, `take_output`, `process_id`, `pause`, `resume`, `shutdown`) - implemented
  for `jerry_host::SessionHandle` in production, and for an in-process, test-only
  `FakeSessionAdapter` (`terminal::pane::pty_pane_fixtures`, a real `futures::channel::mpsc`
  stream a test drives by hand) that finally lets pane tests exercise `PtyOutput::Bytes`/`Exited`
  handling without spawning a real process at all - the DoD's "pane tests against the in-process
  byte adapter" this closes.
- **Resize is the one thing left asking the control plane directly.** `TerminalPane::resize_to`
  still calls `write_input`/`take_output` in-process (data plane, unchanged), but dispatches
  `SessionResize` for the pty's own real size, applying `ResizeLatch::session_resize_succeeded`
  optimistically - immediately, not waiting for the `Report` - since every synchronous caller
  (`maybe_resize_pty`'s own "has this pane settled to its real size yet" check) already expected a
  resize it asked for to be reflected right away, and the old, direct `PtySession::resize` call was
  equally synchronous from that caller's point of view. A failure is logged, not retried
  automatically; the next real resize event (a window resize, a font-size change) retries it.
- **`event/session-exited` gets a real subscriber: `crate::work_surface::session_exited`.** The
  same `worktree_created.rs` pattern (`spawn_consumer`, started alongside it at all three of its
  own call sites in `host.rs`, cancelled with the `HostRuntime` that owns it) - the control-plane
  signal every subscribed client sees, not just the one pane attached to a session's own
  data-plane stream. Its one real reaction: forgets the exited session from the host's agent table
  (`Agents::forget_host_agent`, via a new `Agent::host_session_id` reverse index set once
  `SessionSpawn` resolves), so `AgentsQuery` stops listing an agent whose process has genuinely
  ended even while `Agents::close`'s own "an unclean exit keeps its tab open" policy leaves the tab
  itself in place - this instance's own confirmation that the notification actually reached it,
  rather than assuming the data-plane exit path (which the pane already had) covered everything a
  second, independent client-facing signal is for. The DoD's "`AdeApp` reflects a session exit
  received as an event" test spawns a real, genuinely-exiting process tagged as an agent
  (`Agents::spawn_with_explicit_command_for_test`, since a real `claude`/`codex`/`cursor-agent`
  binary never exits on its own within a test's budget, and this workspace's own nextest
  mitigation deliberately keeps `claude` off `PATH` besides) and asserts the host's own agent table
  goes empty once the real exit's event arrives - not merely that the tab's own data-plane path
  fired, which a bug in the event subscriber specifically would not have caught.
- **A worktree discard now waits out a pane whose spawn was still in flight, not just one already
  attached (GitHub issue #470).** The control-plane `SessionSpawn` round trip genuinely crosses the
  host, unlike the old direct `jerry_pty::spawn` a discard could treat as already settled by the
  time it ran. `TerminalPane::take_session_for_teardown` now also marks the pane `doomed`;
  `TerminalPane::attach_session` checks that flag and, if set, attaches the session but never
  starts its usual output-processing task, leaving it for the discard flow's own poll (via the new
  `TerminalPane::teardown_still_pending`) to claim and shut down *before* `git worktree remove`
  runs, rather than the process attaching after the fact and outliving the worktree it was spawned
  into. The poll yields with a real scheduled no-op task, never `cx.background_executor().timer` -
  GPUI's test scheduler only ever advances its simulated clock against an explicit `advance_clock`,
  which a caller relying on a plain `cx.run_until_parked()` never provides, while a real
  background-thread completion (the host round trip itself) is exactly what `run_until_parked`
  already blocks on with `TerminalPane::new`'s own `#[cfg(test)] allow_parking()` in force.
- **`AdeApp` now genuinely owns no session state.** `Agents` holds `Vec<Agent>` (view state: id,
  kind, cwd, the `Entity<TerminalPane>`, spawn/activity timestamps, the CLI conversation id, and
  now the host session id) plus its `AgentTable` view (`host_agents`); the real `PtySession` for
  every session lives only in `jerry-host`'s own `SessionManager` table. This was already the
  shape once Part A moved the session table and Part B routed every spawn through it - nothing
  further needed removing.

**What still has not moved: the hook store (`hooks/store.rs`) and the rest of `hooks/`'s ~6,300
lines** (`event.rs`, `flow.rs`, `inbox.rs`, `cursor_event.rs`/`cursor_hooks_file.rs`,
`settings_file.rs`, plus their own ~2,200 lines of tests). Assessed, not attempted, in the same
work that did Part B above: moving the store behind a Query plus the existing `event/hook`
notification - so a hook posted to one `jerry-app`/`jerry-cli`/`jerry-mcp` instance is visible to
every other one watching the same host, the same reason `SessionsQuery`/`AgentsQuery` exist - would
mean designing a host-ownable data model for what is currently a `jerry-app`-only, gpui-adjacent
struct, rewriting every read/write site across all six `hooks/` modules to dispatch a Command/Query
instead of touching the struct directly, and updating their ~2,200 lines of existing tests to
match. That is larger than every other change Part B made combined, and belongs in its own issue,
not folded into this one as a partial pass. Tracked as issue #532's own scope, not attempted here.

## 24. `jerry-host` becomes its own process: the binary, spawn-or-connect, lifecycle, version
gating

**Status:** Accepted, fully landed (2026-09-23, issue #506; 2026-09-23, issue #507's own data
plane; 2026-09-24, issue #506's own control-plane cutover, its adapter wiring/error banner/
external DoD test, and a same-day fix so a socket-attached pane's resize is a real dispatch
rather than a silent no-op; plan decisions Q7, Q15, Q20, and §14's spike). The real
protocol/process pieces below, the data plane, the control-plane cutover (`Hosts`/`RepoHost`, one
real per-repository connection with its own event subscriptions, and per-repository
`JERRY_HOST_SOCKET` injection), and the third amendment's closing work (`SocketSessionAdapter`
wired into `Agents::spawn_inner` for real, the per-repository error banner, the external DoD test,
and `ControlPlane`'s own resize fix - see the fourth amendment) are all shipped and tested.

**Context:** Through #505 (§23), the session host is a real, well-factored dispatcher and session
table, but it still runs *inside* `jerry-app`'s own process (`HostRuntime`, §16's `pending_dispatch`
seam) - a socket and a registry entry exist, but the process behind them is `jerry-app` itself.
Stage 3 of the overhaul makes `jerry-host` outlive the app that started it: a real, separate
binary a repository's sessions survive a crash or relaunch of. §14's spike (issue #494, still its
own open issue since its branch never merged - the findings are load-bearing here regardless)
proved the one hard technical question - can a process detach from a job-jobbed parent on Windows
and keep a PTY session alive - passes on all four points; this issue is what actually builds on
that proof.

**Decision, what is real and tested:**

- **`[[bin]] jerry-host` in `crates/jerry-host`** (`src/main.rs`): parses `--repo <common-git-dir>`
  and `--registry-dir <dir>` (a test-only override; production resolves `jerry_core::registry::
  runtime_dir()`), self-adopts a fresh Windows kill-on-close job for its own descendants
  (`src/job_object.rs`, a deliberate copy of `jerry-app`'s own private module rather than a shared
  crate boundary - the same precedent `crate::session::session_manager_tests`' own `process_is_alive`
  pair already set), starts a real `Host`, binds its socket, publishes its registry descriptor only
  once listening, then blocks in `Host::run_lifecycle` until told to stop. Exit code 0 on a clean
  exit. `crates/jerry-host/tests/real_binary_spawn.rs` spawns the actual compiled binary (not an
  in-process stand-in) and proves the whole path for real: publish, a second spawn-or-connect call
  reaching the same process rather than starting a duplicate, and a real `shutdown` stopping it.
- **Host lifecycle** (`Host::run_lifecycle`/`LifecycleConfig`, `crates/jerry-host/src/lib.rs`): a
  host exits when `Inner::is_idle` - no live sessions (`SessionManager::list`) and no fanout sink
  (`Fanout::sink_count`) - holds for `LifecycleConfig::linger` (5s in production, `main.rs`'s
  default), or immediately on an explicit wake. The wake channel (`Inner::shutdown_tx`/`Host::
  shutdown_rx`) is paired at `Host::start_detached` construction time, not when `run_lifecycle` is
  first called - an explicit `shutdown` request arriving in the (real, if narrow) window between a
  socket accepting connections and `run_lifecycle` actually starting must not be silently dropped,
  and pairing the channel eagerly removes that race entirely rather than documenting around it (an
  earlier draft of this decision raced exactly this way under `cargo nextest`'s parallelism, not
  just in theory). `poll_interval` bounds a channel wait rather than a bare loop - the one real
  timer in this crate (§16): idleness is inherently about elapsed time, nothing else to wake on.
- **Control-plane additions to `jerry-core`:** `AppCommand::Shutdown` (`Locality::Session`,
  `Invocability::Denied`, modeled exactly like `SessionSpawn`/`SessionKill` - never actually
  reaches `Command::execute`, `jerry-host`'s dispatcher special-cases it) and `Method::Subscribe`/
  `Request::Subscribe` (`event/subscribe`, the one client-to-host method in the `event/`
  namespace - every other `Event(_)` name stays host-to-client only, refused as an incoming
  request exactly as before). Both have real fixtures (`fixtures/request-command-shutdown.json`,
  `fixtures/request-subscribe.json`) and round-trip through `Call::examples`'s existing mechanical
  fixture test; `Shutdown` appears in the MCP tool catalogue (denied to agents, same as every
  other Session-locality command), `Subscribe` does not (`crate::mcp::tool_for` excludes it - it
  opens a stream, not a typed call/response). Deliberately, `dispatch::handle`'s `Shutdown` arm
  does **not** call `Inner::request_shutdown` itself - see "The `Shutdown` reply must be written
  before the wake it causes" below for why that call moved to each transport instead.
- **The `Shutdown` reply must be written before the wake it causes.** The first cut called
  `Inner::request_shutdown` inside `dispatch::handle`, synchronously, before the reply had even
  been handed to the socket's writer thread - a real bug caught by CI (Linux only; macOS/Windows
  happened to flush first): `Host::run_lifecycle` woke, `main`'s `Host::shutdown` ran, and
  `Inner::close_connections` could sever the requesting socket before its own `Report::Ok` had
  been written, so the client saw `Frame(Closed)` instead of its answer. Fixed by moving the
  trigger to each transport, once it has actually seen its own reply go out: `LocalClient::call`
  fires it right after `Inner::submit` resolves (no separate flush step to race for an in-process
  caller); `listener::serve`'s reader thread instead records the pending response's `RequestId` in
  a small `Mutex<Option<RequestId>>` shared with the writer thread, and the writer thread - the
  one place that knows `write_frame` (which flushes) has actually completed for that exact
  message - fires the wake immediately after, never before. Regression-tested by
  `lifecycle_tests::a_shutdown_reply_always_arrives_before_the_socket_closes`, which repeats the
  real spawn-a-host-and-shut-it-down sequence ~20 times against a real socket, since a single pass
  rarely reproduces a thread-scheduling race. `Host::shutdown` called directly (not through the
  `Shutdown` command at all) now also calls `Inner::request_shutdown` itself, so the same "wakes
  a concurrently running `run_lifecycle`" guarantee holds for that path too
  (`lifecycle_tests::calling_shutdown_directly_also_wakes_a_concurrently_running_lifecycle_loop`).
- **`event/subscribe` gates the fanout, in `jerry-host`'s listener, not the dispatcher.** Every
  socket connection used to become a fanout sink automatically on accept
  (`Fanout::subscribe_socket`, unconditionally, in `listener::serve`); now a connection is a sink
  only from the moment it sends `event/subscribe` (recognized directly in the reader loop, before
  `Call::from_wire` - the listener already has the connection's own `outbox` in scope, which
  `dispatch::handle` running on a shared dispatch thread does not). This is what makes `Inner::
  is_idle`'s "no connected client" half meaningful: a one-off `jerry status` request/response call
  no longer counts as "connected" and no longer keeps a host alive past its linger.
  `listener::socket_tests::a_connection_that_never_subscribes_receives_no_push_events` is the
  regression test for the gate itself.
- **Spawn-or-connect, in `jerry_core::host_spawn`** (a new module, `jerry-pty` promoted from
  `jerry-host`-only to a real dependency of `jerry-core` for this): `spawn_or_connect(registry_dir,
  repo, timeout, spawn_deadline)` resolves the registry for `repo` (already the common `.git`
  dir - `Ctx::repo_path` already is one for every real caller), connects to a live, version-matched
  host, reports `Outcome::VersionMismatch { descriptor }` for a live but wrong-version one (never
  connects), or spawns `jerry-host --repo --registry-dir` detached and polls the registry by real
  connect attempts (never a bare timer) up to `spawn_deadline`. One implementation, not two: both
  `jerry host start` and (once wired - see below) `jerry-app`'s own `HostRuntime` are meant to call
  this.
- **`jerry-core` stays free of this workspace's Win32 job-object FFI entirely - CLAUDE.md's
  unsafe list is unchanged.** `jerry_pty::new_detached_command` sets `CREATE_BREAKAWAY_FROM_JOB`/
  `CREATE_NEW_PROCESS_GROUP` on Windows and `process_group(0)` on unix - both safe `std` wrapper
  calls, no `unsafe` - but *detecting* a forbidding job needs the real
  `IsProcessInJob`/`QueryInformationJobObject` read (§14 point 2), which is `unsafe` FFI. An
  earlier draft put that read in `jerry-pty` itself; moved instead to
  `jerry_app::job_object::breakaway_is_forbidden_for_current_process`, alongside the sanctioned
  kill-on-close job code CLAUDE.md's unsafe list already names, and to `jerry-host`'s own
  `job_object.rs` as the second sanctioned home if that binary ever needs the same check for
  itself. `host_spawn::spawn_or_connect_with` takes this as an injected `breakaway_forbidden: impl
  Fn() -> bool` closure, called only once a real spawn attempt has already failed, to classify
  that failure - never to decide whether to attempt the spawn. The public `spawn_or_connect`
  passes `|| false` (a plain `jerry host start` from a shell has no job of its own worth
  checking); a caller that can tell injects its own `job_object::
  breakaway_is_forbidden_for_current_process` instead and gets the specific
  `SpawnOrConnectError::BreakawayForbidden`, never a silent in-job fallback spawn.
- **`Registry::claim` closes the gap between `resolve` answering `None` and a caller actually
  spawning.** Without it, two callers racing `spawn_or_connect` for the same repository could both
  see no live host and both spawn one. `claim(repo, max_age)` creates `<hash-of-repo>.spawning` via
  `create_new` - exactly one racing caller gets `Ok(true)` and proceeds to spawn; the rest get
  `Ok(false)` and wait for the winner's descriptor instead (`spawn_and_wait`'s own
  `wait_for_descriptor` helper, reused either way). A claim older than the spawn deadline is
  stale - its holder must have died mid-spawn - and is swept and retried, the same "sweep on
  discovery" pattern `Registry::resolve` already uses for dead descriptors. The hash is a plain
  FNV-1a over the canonical repo path, not `RandomState` (per-process keyed, unsuitable - two
  processes must compute the same file name) or `DefaultHasher` (no cross-version stability
  guarantee). `registry_tests::exactly_one_of_two_concurrent_claims_for_the_same_repository_wins`
  is the regression test: two real threads racing `claim` against one registry directory.
- **`jerry_core::jerry_binary::locate_named`**, generalizing the existing `jerry`-only lookup to
  any sibling binary (`jerry`, `jerry-host`) by the same three tiers (sibling, `bin/`, one
  directory up); `locate()` is now `locate_named("jerry")`.
- **`jerry host start`/`jerry host stop`** (`jerry-cli`): `start` calls `host_spawn::spawn_or_
  connect` against the caller's own `cwd`-resolved repository and prints the resulting socket path
  (or the version-mismatch/breakaway-forbidden message on stderr); `stop` sends `Shutdown` through
  the ordinary `Session::call` path - Standalone reports "nothing to stop" rather than an error.
  Tested two ways: `crates/jerry-cli`'s own `run_tests` against an in-process `Host` (the
  `[[bin]]`-free path - `CARGO_BIN_EXE_jerry-host` is only set for `jerry-host`'s own integration
  tests, not `jerry-cli`'s), and the real-binary path in
  `crates/jerry-host/tests/real_binary_spawn.rs`.
- **Bundle scripts and the release workflow ship `jerry-host` next to `jerry`/`jerry-app`**:
  Windows `bin\jerry-host.exe`, macOS `Jerry.app/Contents/MacOS/jerry-host`, Linux `bin/jerry-host`
  (staged, installed by `install.sh`, and covered by `codesign --deep`'s existing bundle-wide
  signing pass on macOS - no per-binary script changes needed there beyond staging the file).

**Amended 2026-09-23 (#507, part 1 - the data plane and its app-side adapter, landed real and
tested; the control-plane cutover below did not move in this same pass):**

- **`command/session-attach`, in `jerry_core::session`** (`SessionAttach`/`SessionAttachOutcome`):
  `Locality::Session`, `Invocability::Denied`, modeled like `SessionSpawn` - never actually reaches
  `Command::execute`, `jerry-host`'s dispatcher special-cases it. Answers `{ socket: PathBuf }`,
  the path to a real, already-listening per-session socket; a second attach while one is live
  answers `Report::Denied { code: "session-already-attached", .. }`. Has a real fixture
  (`fixtures/request-command-session-attach.json`) and round-trips through `Call::examples`'s
  mechanical test like every other variant. `SessionRecord` gained a `process_id: Option<u32>`
  field (set once, at spawn, from the real `PtySession`) so a caller can resolve a session's pid
  from `SessionsQuery` instead of a control-plane round trip of its own.
- **The data-plane socket itself, in `jerry-host`'s new `crate::data_plane` module**: one real
  AF_UNIX/named-pipe socket per session, bound the moment `SessionManager::spawn` starts the
  session (named `d-<pid>-<fresh_u32>.sock` under the same directory the host's own control socket
  lives in - `SessionManager::new` now takes a `sockets_dir`, and `Host::start_detached`/
  `Host::start_at` thread it through; `Host::start`/`Host::start_with` keep their old,
  parameterless shape via a new `jerry_host::default_sockets_dir()` fallback for the many test call
  sites that never cared before). From the moment a client connects, the byte stream is raw and
  bidirectional - the session's own relay thread (`crate::session::spawn_relay`) pushes every
  `PtyOutput::Bytes` chunk to the data plane the same instant it forwards it on the existing
  `SessionHandle` path, and a connected socket's reader thread forwards its own bytes straight to
  `PtySession::write_input`. Output produced before any client attaches is buffered in a bounded
  (**256 KiB**, a named constant, `PRE_ATTACH_BUFFER_CAP_BYTES`), drop-oldest ring, replayed to
  whichever connection attaches next - including one that attaches *after* the session has already
  exited, draining the tail and then closing, rather than only covering the narrower "attaches a
  moment late" case. "Exactly one attach at a time" is enforced for real at the socket layer (a
  second real connection while one is already being served is accepted and dropped immediately),
  with `SessionManager::attach`'s own `Report::Denied` an earlier, friendlier rejection of the
  common case - a real TOCTOU gap between the two is possible in principle and is closed by the
  socket layer, not papered over. The host closes the connection once the session exits and every
  already-buffered byte has been delivered (`DataPlane::mark_exited` drops the live sink, which is
  what makes the writer thread finish draining and close). Tested against a real shell, driven
  entirely over the socket (never `SessionHandle::write_input`/`take_output` directly, which would
  prove nothing about the path a real client actually takes): `session::data_plane_tests` in
  `crates/jerry-host` covers bytes both ways and a real close-on-exit, a second attach denied until
  the first disconnects and allowed again after, and pre-attach buffering (the real ConPTY startup
  handshake bytes on Windows, since nothing answers a one-shot command's own Device Status Report
  query with nobody attached - see that module's own docs for why the test differs by platform
  there).
- **`SocketSessionAdapter`, in `crates/jerry-app/src/terminal/socket_adapter.rs`**: the second
  implementer of `crate::terminal::pane::SessionAdapter` besides `jerry_host::SessionHandle`'s
  in-process one - a reader thread forwarding socket bytes as `PtyOutput::Bytes` items into a
  channel (channel-woken, never polled), `write_input` writing straight to the socket, `process_id`
  answered from the `SessionRecord::process_id` a caller resolved via `SessionsQuery` before
  constructing it (never its own round trip), and `shutdown` dispatching `command/session-kill`
  through a caller-injected closure before closing its own connection. `pause`/`resume` answer a
  real, honest error rather than a silent no-op: pausing a session's real process is not yet a
  control-plane command an out-of-process session can ask its host for. Unit-tested against a real
  socket pair with no `jerry-host` involved (`socket_session_adapter_tests`): bytes in order,
  `write_input` reaching the peer, `shutdown` calling the injected kill and still closing when that
  kill itself fails, and the stream simply ending (no synthesized `Exited`) when the host closes
  its side. `jerry_pty::PtyError` gained one new variant, `Remote(String)`, for exactly this
  adapter's socket-I/O and kill-dispatch failures - it has no real `PtySession` to report any of
  the other variants for.
- **`Agents::spawn_inner` still attaches via `SessionManager::handle_for`, not the socket -
  attempted, reverted, and left as this module's own documented follow-up.** A first pass had
  `spawn_inner` dispatch `command/session-attach`, connect a `SocketSessionAdapter` off the UI
  thread, and hand that to `TerminalPane::attach_session` instead of the in-process handle - every
  step of that chain genuinely worked (confirmed live, one step at a time: the extra dispatches
  resolved, the socket connected, the real shell's own ConPTY startup bytes arrived through it and
  rendered correctly, a resize dispatched through the same path succeeded). With that wiring in,
  every `#[gpui::test]` that spawns a real session and lets it settle at an interactive prompt
  reproducibly hung *inside `TestAppContext::run_until_parked` itself*, after every one of the
  above had already completed and logged as much - not a busy loop (confirmed via `Get-Process`:
  zero CPU growth across repeated samples, a genuine block, not a spin) and not explained by any
  difference this entry could pin down between this adapter's reader/writer threads and
  `jerry_pty`'s own identically-shaped ones (down to the same `futures::executor::block_on(tx.
  send(chunk))` on a bare `std::thread`, which the latter's own hundreds of passing real-pty tests
  already exercise). Reverted rather than shipped or worked around by rewriting the affected tests'
  own wait idiom, per CLAUDE.md's "no fake functionality": a data plane a client cannot actually
  attach to from the one production call site that would use it is not yet a real cutover, and a
  hang this reproducible is not something to paper over with a different polling shape in code this
  change does not otherwise need to touch. `SocketSessionAdapter` and `command/session-attach`
  themselves are unaffected - both are real and independently tested (this entry's own paragraphs
  above) - only `Agents::spawn_inner`'s choice of which adapter to construct reverted, to the exact
  shape decisions.md §23 already described. Root-causing the `run_until_parked` interaction, and
  the pane's own exit signal moving to `event/session-exited` for a socket-attached session
  (`TerminalPane::mark_exited_from_event`, also reverted here since nothing constructs the adapter
  that would need it), are this follow-up's own scope together with the control-plane cutover
  below - wiring either one in before the other is fully real would be the same half-wired state
  this paragraph just described avoiding.

**The control-plane cutover was scoped to a follow-up PR (`feat/506-control-plane`) rather than
folded into the data-plane one above - deliberately:** `jerry-host` itself serves exactly one
repository per process (`--repo <path>`, singular; a stage-3 host's own `Descriptor.repos` is
always a one-element list), while `jerry-app`'s old, single `HostRuntime` served *every*
repository the app had open on one shared in-process connection. Cutting `AdeApp::dispatch` over
to real spawn-or-connect was therefore not a drop-in transport swap behind the existing
`HostRuntime::client()`/`host_client()` call sites; it was a real, separate architectural change to
`AdeApp`'s own repo/host bookkeeping, verified correct across every existing repo-scoped test
rather than rushed to compile in the same pass as the data plane, per CLAUDE.md's "no fake
functionality" rule.

**Amended 2026-09-24 (#506 part 2 - the control-plane cutover itself, landed real and tested; the
adapter wiring, error-banner UI, and external DoD test below remain open):**

- **`Hosts`/`RepoHost`, one real connection per open repository, replacing `HostRuntime`
  entirely** (`crates/jerry-app/src/host.rs`): `Hosts { by_repo: HashMap<PathBuf /* common `.git`
  dir */, RepoHost>, common_dir_of: HashMap<PathBuf, PathBuf> }`. `RepoHost` wraps a `Connection`
  (`Connection::Remote(RemoteRepoHost)` in production, `Connection::InProcess { host, client }`
  `#[cfg(test)]`-only) plus a `RepoHostState` (`Connected`/`VersionMismatch { theirs, ours }`/
  `CannotSpawn { error }`) and its own three event-subscription tasks. `ensure_repo_host_connected`
  resolves `cwd` to its repository's common `.git` dir (falling back to `cwd` itself for a plain,
  non-git directory - `test_support::temp_root`'s own supported case, and the reason `SessionSpawn`
  never needed git resolution server-side either), opens a connection only the first time that
  repository is seen, and wires its event subscriptions - all off the UI thread.
  `AdeApp::open_repo_host` calls it eagerly from `add_repo`; `AdeApp::dispatch`'s own cache-miss
  path calls the identical function lazily, so a worktree created directly on disk or dispatched
  into before the eager call resolves still gets a real connection rather than a permanent
  `NEEDS_HOST` - `app_dispatch_tests::two_open_repositories_dispatch_through_two_different_hosts`
  and `dispatch_lazily_opens_a_connection_for_a_cwd_never_explicitly_opened` are the regression
  tests for the two convergent paths.
- **`RemoteRepoHost`, in the new `crates/jerry-app/src/repo_host.rs`** - not in the original plan
  text, added because of a real deadlock this cutover's own work surfaced: a dedicated OS worker
  thread owns one `jerry_core::client::Client`; `dispatch(&self, call) -> Result<Report, RpcError>`
  is a plain blocking `std::sync::mpsc` round trip, safe to call from *any* context, including
  inside a `cx.background_spawn` task under GPUI's single-threaded test scheduler
  (`dispatch_is_safe_from_inside_a_background_executor_task_under_the_test_scheduler` is the
  regression test) - see §16's own amendment for why the plan's plainer sketch (`RepoHost { client:
  jerry_core::client::Client, .. }`, called directly from a background task) would have deadlocked
  the exact same way `SocketSessionAdapter`'s first cut did. `subscribe_remote` is this module's
  second piece: a dedicated *second* connection per repository, sending `event/subscribe` and
  forwarding `event/*` notifications from a bare OS reader thread into a channel - the real
  out-of-process twin of `jerry_host::LocalClient::subscribe`.
- **Three event subscriptions per repository, not one shared app-wide fanout**:
  `worktree_created`/`session_exited`/`event/hook`, each its own dedicated socket in production
  (`subscribe_remote`, called twice more for the second and third), the in-process fanout tapped
  three times in tests. All three feed the same app-wide handlers regardless of which repository an
  event came from - `crate::work_surface::worktree_created::spawn_consumer`,
  `crate::work_surface::session_exited::spawn_consumer`, and the new `crate::hooks::spawn_consumer`
  all share the identical channel-woken-`cx.spawn`-task shape.
- **`crate::hooks::HookRuntime` keeps no host connection of its own.** It lost its `host_socket`
  field and its own internal notification-consuming task entirely - `HookRuntime::start` now only
  writes the `--settings` file and holds the inbox/edit log, needing neither a client nor a `cx`.
  `event/hook` reaches it through whichever repository's own subscription (above) happens to be
  forwarding one, via `HookRuntime::record`. `HookRuntime::injection(host_socket)` takes the
  spawning repository's own socket as a parameter, resolved fresh per spawn by
  `AdeApp::hook_injection_for` through `AdeApp::host_socket_for(cwd)` (`Hosts::socket_for`) - so an
  app with two repositories open injects each agent's `JERRY_HOST_SOCKET` as *that* repository's
  own socket, never a single app-wide one. Bring-up (writing the settings file, keyed on a one-shot
  `hook_runtime_tried` flag) no longer depends on any host connection at all - only the
  per-repository socket lookup does, and that is retried on every spawn rather than baked into the
  one-shot flag, so a repository whose connection is still resolving simply falls back to the
  terminal-title/quiescence signals for that one spawn instead of permanently disabling hooks over
  a race.
- **Agent identity crosses the process boundary atomically, closing a real race the in-process
  design never had to face.** `SessionSpawn` gained `agent: Option<SessionAgentInfo { kind,
  agent_id }>` (`jerry-core`), so a session's agent association is made in the same call that
  creates the session record - there is never a window where a hook from that agent id could
  arrive before its registration exists, the way a separate post-spawn `AgentTable::register` call
  would allow across a real socket. `SessionManager::agent_is_live`/`worktree_of_agent`/
  `agent_entries` all scan real session records by this field (preferring live over exited, and
  filtering exited sessions out of `agent_entries` - which is what lets `AgentsQuery` stop listing
  an agent the moment its real session exits, with no client action needed at all). A second
  `SessionSpawn` reusing a still-live agent id is denied (`Report::Denied { code: "agent-id-taken",
  .. }`) rather than silently accepted. `jerry-app`'s `Agents` correspondingly lost its own
  in-process `AgentTable` shortcut (`attach_host`/`forget_host_agent` removed) - it now passes
  `agent: Some(SessionAgentInfo { .. })` straight into every `SessionSpawn` it sends, the same way
  a real out-of-process caller would have to.
- **`HostRuntime::in_process()`/`LocalClient` dispatch are `#[cfg(test)]`-only**, exactly as
  planned: the test app's `Hosts` gets one in-process `RepoHost` per opened repository
  (`start_in_process_repo_host`), so existing UI tests keep passing unchanged, plus
  `app_dispatch_tests::two_open_repositories_dispatch_through_two_different_hosts` proves two open
  repositories reach two different hosts - closing this issue's "one execution path" DoD for the
  control plane, the data plane's own half already closed by the amendment above.
  `AdeApp::sessions_for`/`host_client_for` remain real only for a `#[cfg(test)]` in-process
  repository (documented gap: `Agents::spawn_inner`'s own migration to `SocketSessionAdapter`,
  below, is what would make these meaningful in production too).

**Amended 2026-09-24 (#506 part 3 - adapter wiring, the error banner, and the external DoD test,
closing this issue's own remaining scope):**

- **`Agents::spawn_inner` now attaches through `SocketSessionAdapter` for a production
  `Connection::Remote` repository, real for the first time.** `crate::host::attach_remote_session`
  is the new seam `Agents::spawn_resolved` falls back to whenever `AdeApp::sessions_for` answers
  `None` (a production repository, or a test that swapped one in via `RepoHost::for_test_remote`):
  it dispatches a real `command/session-attach` (through the ordinary `AdeApp::dispatch`, off the
  UI thread) for the socket, a `SessionsQuery` for the session's own pid
  (`SocketSessionAdapter::connect`'s own docs - a value only a Query can resolve), then connects
  over the socket off the UI thread. `SocketSessionAdapter::shutdown`'s `command/session-kill`
  dispatches through `AdeApp::remote_host_for`'s `RemoteRepoHost` directly - a plain blocking call
  to a dedicated worker thread, never through `AdeApp::dispatch`/GPUI's executor, since `shutdown`
  is a synchronous trait method with no `.await` of its own to run one under (§16's amendment is
  exactly why this distinction matters). The `#[cfg(test)]` in-process path
  (`SessionManager::handle_for`) is unchanged. Regression-tested by `work_surface::agents::
  remote_attach_tests::a_real_idle_shell_over_a_remote_connection_attaches_and_closes_without_
  hanging` - a real, out-of-process `RepoHost::for_test_remote` connection, a real idling shell
  attached and settled at its prompt, then closed through `Agents::close`'s own real
  `cx.background_executor().spawn(session.shutdown())` shape, the exact call the original,
  reverted wiring hung inside forever. It does not; nextest's own slow-timeout would have caught it
  if it still did.
- **The per-repository error banner is real, in `crate::rail::render`.**
  `AdeApp::repo_host_state_for(cwd)` (a clone of `RepoHost::state()`) gates
  `render_repo_host_error_banner`, shown in the rail alongside the existing worktree-listing and
  selection-notice banners for the currently focused repository; `RepoHostState::error_message`
  gives its text. "Restart sessions" calls `AdeApp::restart_repo_host`, which drops both the stale
  `Hosts::by_repo` entry and its `common_dir_of` cache entry (leaving the latter behind would make
  `Self::open_repo_host`'s own early-return guard silently no-op the restart) and re-opens the
  repository exactly as if this were the first time this instance had ever seen it.
  `rail::render::repo_host_banner_tests` covers both the gating (connected vs. broken) and the
  restart's own real effect.
- **The external DoD test - a session surviving its spawning client's own disconnect, with bytes
  still flowing - is written and has been run here, on Windows.**
  `crates/jerry-host/tests/real_binary_spawn.rs`'s `a_session_survives_its_spawning_clients_
  disconnect_and_a_fresh_client_still_reaches_it`: a real, separate `jerry-host` binary, a real
  spawned shell, real bytes over the real data-plane socket both before and after the original
  client - both its data-plane stream and its control-plane `Client` - disconnects without ever
  sending `Shutdown`. The host is confirmed still alive (`registry::probe`, never `Liveness::
  Dead`), a fresh `Client` reconnects and finds the same session still alive through a real
  `SessionsQuery`, and a fresh attach still carries real bytes both ways. Not `#[ignore]`d: unlike
  an `external` `#[ignore = "external: <binary>"]` test, this needs no third-party binary, only
  `jerry-host`'s own already-built one - the same reasoning `real_binary_spawn.rs`'s existing
  `spawn_or_connect_starts_a_real_jerry_host_binary_and_a_second_call_reuses_it` test already
  applies - so it is a permanent regression test in the ordinary gate, not a one-off manual check.

**Amended 2026-09-24 (#506 part 4 - a real `Host::listen` bug found by PR #539's own Linux/macOS
CI, and the resize fix the third amendment above deferred):**

- **`Host::listen` now creates its own socket's parent directory before binding**
  (`jerry_core::registry::ensure_private_dir`, the same call `DataPlane::bind` already made for a
  per-session socket - `Host::listen`'s own control-plane bind had never made the identical call).
  A fresh CI runner's own runtime/socket directory does not exist until something creates it;
  production's own bind is covered by `Registry::open` doing so moments earlier, but the
  `#[cfg(test)]` in-process `RepoHost` (`start_in_process_repo_host`) binds directly under
  `jerry_host::default_sockets_dir()` with no such call - Windows CI passed only because an
  earlier test's own run had already created the shared directory, while a fresh Linux/macOS
  runner's `listen()` failed outright, turning every dispatch in that test app into a `NEEDS_HOST`
  error - the same shape `#[cfg(test)]`'s own start-up path modeled as `RepoHostState::
  CannotSpawn` rather than surfacing loudly. `start_in_process_repo_host` now panics on that
  failure instead: a throwaway test host failing to bind is always a fixture bug, never a real
  state a test should have to model (`RepoHost::for_test_unavailable`/`adopt_repo_host_for_test`
  remain the real, deliberate way to construct a broken connection for the error-banner tests
  themselves). `jerry-host`'s own `listen_creates_its_own_socket_directory_when_it_does_not_exist_
  yet` is the regression test.
- **`ensure_repo_host_connected`/`adopt_repo_host_for_test` now `dunce::canonicalize` the resolved
  common directory before using it as `Hosts::by_repo`'s key.** `jerry_git::git_common_dir`
  absolutizes `--git-common-dir`'s own (often relative) output against whatever spelling of `cwd`
  it was given, so a worktree reached through a symlinked parent (macOS's own `/var` ->
  `/private/var`) and the repository's own already-canonical root resolve to two different
  strings for the same real directory - a second, independent (and, in a test, session-less)
  `RepoHost` opens for the mismatched spelling instead of reusing the first. Two regression tests
  in `host::app_dispatch_tests`: a real symlinked parent (gracefully skipped where the environment
  refuses unprivileged symlink creation) and a portable redundant-path-component case that runs
  everywhere, both proving a session spawned through the alternate spelling is visible through the
  canonical one.
- **`TerminalPane` no longer holds a bare `LocalClient` at all - `ControlPlane` replaces it**,
  closing the third amendment's own deferred gap: resizing a socket-attached (production) pane was
  a silent no-op, since `SessionResize` had no path for that connection kind. `ControlPlane`
  (`crate::terminal::pane`) is a small enum - `Remote(RemoteRepoHost)` in production, `InProcess
  (LocalClient)` in tests - handed to a pane at attach time (`AdeApp::control_plane_for`, the same
  shape `attach_remote_session`'s own kill closure already used) and dispatched off the UI thread
  exactly as before: a plain blocking `RemoteRepoHost::dispatch` for the real case (safe from any
  context, §16's amendment), a real `.await` over `LocalClient::request` for the in-process test
  case (never a `block_on`, so equally safe to run from inside a background task). `SessionRecord`
  gained `rows`/`cols`, set at spawn and updated on every real `SessionManager::resize` - what a
  socket-attached pane's own resize test verifies against, since it has no in-process `PtySession`
  to read the applied size back from directly.
  `terminal::pane::control_plane_resize_tests::a_socket_attached_panes_resize_reaches_the_real_
  host` is the regression test: a real, separate-socket-shaped `RemoteRepoHost`, a real spawned
  shell, a real `TerminalPane::resize_to` call, and a fresh `SessionsQuery` confirming the applied
  size landed at the host.

This closes every item this issue's plan named as its own scope, including both items the third
amendment above had deferred.

**Latent, noted rather than fixed here:** a `Connected` `RepoHost`'s three event-subscription
threads (`worktree_created`/`session_exited`/`event/hook`) are never torn down, because nothing
in production ever removes an entry from `Hosts::by_repo` once opened - there is no
`remove_repo`/"close this repository" caller yet. Harmless today (a repository stays open for the
app's whole lifetime), but whichever issue first needs to actually drop a `RepoHost` (issue #507's
own detached-sessions work, or a later one) must make `RepoHost`'s `Drop` shut down its
subscription sockets so those threads actually exit, not just leave the entry unreachable while
its threads keep running. **Still latent after #507:** that issue's own detached-sessions work
(§25 below) never adds a "close this repository" caller either - a repository stays open, and its
`RepoHost` alive, for as long as the window that opened it does, exactly as before. Whichever
issue first needs one still owns this fix.

## 25. The host owns the terminal grid; reattach is a snapshot, never a raw byte replay

**Status:** Accepted, landed across three PRs on issue #507 (2026-09-24: `jerry-term` extracted;
the snapshot mechanism; reconnect-on-launch reconciliation; quit semantics; the external DoD test).

**Context:** §24's own data-plane amendment gave a session a real socket, but a client attaching
after bytes had already been produced only ever saw a **raw byte buffer replay** (§24's own
`PRE_ATTACH_BUFFER_CAP_BYTES`, 256 KiB, drop-oldest) - real for the narrow "attached a moment
late" case that amendment scoped itself to, but two real problems once reattach became the point
of the exercise rather than an edge case: replayed bytes render at whatever size the pty was *at
the moment they were produced*, not the client's current one (a resize between detach and
reattach leaves the replay visibly wrong until the child reacts on its own), and a buffer capped
at 256 KiB has no real answer for "I detached hours ago, what's on screen now" - it holds recent
bytes, not current state. Both problems are exactly what Zed's and Warp's own reattach
implementations independently hit and abandoned raw replay over, per their own public design
notes at the time this issue was scoped.

**Decision:**

- **`jerry-term`, a new gpui-free crate, is the one VT/grid engine both the app and the host
  use.** Extracted verbatim from `crates/jerry-app/src/terminal/{grid,mouse,osc}.rs` - it never
  depended on `gpui` even while it lived there - and re-exported wholesale by
  `crate::terminal::{grid,mouse,osc}` so no call site elsewhere in `jerry-app` had to change.
  `jerry-host` takes it as a real (non-dev) dependency for its own headless grid, below.
- **Every session gets a real, headless `jerry_term::grid::TerminalGrid` on the host, fed the
  identical bytes the data plane is.** `SessionManager::spawn`'s relay thread
  (`crate::session::spawn_relay`) calls `grid.append_bytes(chunk)` on every `PtyOutput::Bytes`
  item, in the same order it forwards them to the data-plane socket and the in-process
  `SessionHandle` - the host's own copy of "what does this pty's screen actually look like right
  now" is never behind, never ahead, of what a live client would have rendered itself.
  `SessionManager::resize` resizes this grid alongside the real pty, so the headless grid's own
  geometry is always the session's real, current one - not a snapshot of whatever it was sized at
  when this client's own attach happened to run.
- **`command/session-attach` answers `{ socket, snapshot }`, never just a socket.**
  `SessionAttachOutcome` gained `snapshot: jerry_core::SessionSnapshot` - `rows`/`cols`, the real
  `cursor` position (`None` when hidden), `cells` (exactly `rows * cols`, row-major) and
  `scrollback` (real retained history above the visible screen, oldest first, capped at
  `SNAPSHOT_SCROLLBACK_CAP_LINES` = 200 lines - bounded so a long-lived session's own history
  never makes an attach answer unbounded). `SnapshotCell`/`SnapshotCellWidth` are `jerry-core`'s
  own wire twins of `jerry_term::grid::GridCell`/`CellWidth`, kept in `jerry-core` rather than a
  `jerry-term` dependency so that crate stays a pure wire-contract type with no `gpui`-adjacent
  dependency at all (§4/§15) - `jerry-host` converts field-by-field at its own boundary
  (`session::snapshot_from_grid`/`convert_cell`). The byte stream on the returned socket starts
  exactly at the snapshot point - never replayed - and §24's own pre-attach buffer
  (`PRE_ATTACH_BUFFER_CAP_BYTES`, `DataPlane`'s own `buffer` field) is gone outright, not merely
  unused: a client attaching late now has a real, current, correctly-sized answer instead of a
  bounded tail of history to catch up through.
- **A snapshot's `cells` are the grid's own raw, unhighlighted content - never the live cursor's
  inverse-video swap baked in.** `TerminalGrid::visible_rows` (the app's own live-render path)
  swaps a cell's fg/bg when it is the cursor's current position, so the renderer never needs a
  separate cursor glyph; `visible_rows_plain` is the identical read without that swap, and is
  what `snapshot_from_grid` uses. Baking the swap into a wire snapshot's own stored colors would
  double it away to nothing the moment a client's own live render (over its own seeded grid,
  below) reached that same cell and swapped it *again* - a real bug caught before it shipped by
  the seeding round-trip test that renders a seeded grid live and diffs it against the original.
- **A client seeds its own grid from a snapshot by synthesizing minimal ANSI through the same VT
  parser every other byte goes through - never a special-cased "static picture" mode.**
  `TerminalGrid::seed_from_snapshot(cursor, scrollback, cells)` prints `scrollback` then `cells`
  (oldest first) as real SGR-styled text through `Self::append_bytes`, letting more lines than
  the screen holds naturally push the oldest into real retained history exactly as a live
  program's own output would, then positions the real cursor with a real CSI sequence (or hides
  it with real `DECTCEM` off). The result is a grid whose `Term` genuinely holds that content, so
  any further live bytes the reattached socket delivers apply against real state - a resize, a
  redraw, a program that overwrites part of an old line - rather than landing on an empty screen a
  "just show this picture until the first real byte arrives" shortcut would have produced.
  `SocketSessionAdapter` carries the attach's own snapshot (`SessionAdapter::snapshot`, `None` for
  a freshly-spawned in-process `SessionHandle`, whose grid already starts empty and correct) and
  `TerminalPane::seed_from_snapshot` seeds before consuming a single byte off the adapter's output
  stream, latching `ResizeLatch`/`settled_real_size` to the snapshot's own dimensions so the
  pane's own later real-content-box resize correction can never mistake the snapshot's real,
  just-seeded history for placeholder-size garbage and discard it (`TerminalPane::
  maybe_resize_pty`'s existing "first real resize discards inherited placeholder scrollback"
  rule, which must never fire for a reattached pane at all).
- **Reconnect-on-launch reconciles the persisted tab layout against a real `SessionsQuery`
  answer** (decision Q21 of the plan this issue is part of), in
  `crate::work_surface::session_reconcile` (gpui-free, `jerry-app`-internal) - one of three
  outcomes per persisted tab: **Reattach** (its recorded `host_session_id` still answers - attach
  in place, `Agents::spawn_reattached`, painting the real snapshot immediately), **Gone** (the id
  was recorded but the host has no live session for it - shown already-exited with the host's own
  last-known status, `Agents::spawn_gone` -> `TerminalPane::mark_restored_as_gone`, which
  deliberately never emits `TerminalPaneEvent::ProcessExited`: the existing "a process that
  finished cleanly closes its own tab" convenience is right for a live run the user watched end,
  and wrong for a placeholder appearing already-finished the instant it's restored - emitting it
  auto-closed the tab before this fix, a real regression the reconciliation's own end-to-end test
  caught), or **Fallback** (no recorded id at all - the pre-existing spawn/`--resume` path,
  unchanged). A live session under the worktree that answers to no persisted tab lands on
  `AdeApp::detached_sessions`, surfaced by the rail's own "Detached sessions" section; opening one
  reattaches it as a real tab (`AdeApp::open_detached_session`). `PersistedTab`/`SessionTab::
  {Shell,Agent}` gained `host_session_id` to make any of this possible at all.
- **Quit detaches by default; stopping every host is a separate, deliberate action.** Every
  repository's session host is already its own process (§24) - an ordinary `cx.quit()` was
  already correct "detach" behavior with no code change needed, since nothing in `jerry-app` ever
  sent a live repository's host a `Shutdown` on the app's own exit. `MenuCommand::
  QuitAndStopAllAgents`/`root::QuitAndStopAllAgents` (decision Q16) is the one path that isn't:
  `crate::root::menu_commands::quit_and_stop_all_agents` enumerates every open window
  (`gpui::App::windows`), calls each one's own `AdeApp::shutdown_all_repo_hosts_blocking` (a
  plain blocking `RemoteRepoHost::dispatch` of a real `Shutdown` per connected repository host,
  safe from any context per §16's amendment - there is no async context to await one from at this
  call site anyway), then quits. Registered as a global `on_action` listener (no window in scope,
  the identical reason `Quit`/`Hide`/`Hide Others`/`Show All` already are) since it has to reach
  every open window's repositories, not just whichever one dispatched it - wired into the macOS
  application menu, the Windows/Linux popover's own exhaustiveness check, and the command palette.
- **Which drop disconnects and which kills, made explicit.** A plain drop of `AdeApp`, a window,
  a `TerminalPane`, or a `SocketSessionAdapter` must only ever disconnect - close whatever control-
  or data-plane connection that value owned - never reach into `jerry-pty`/`jerry-host` to end the
  real process on the other side of it. `SessionAdapter::shutdown` (an explicit call, never a
  `Drop`) is the one thing that kills, and only two callers ever make it: `Agents::close` (closing
  a tab) and the worktree-discard flow - both real, deliberate user actions, never app/window/pane
  teardown. `SocketSessionAdapter` had no `Drop` impl at all until this issue: closing a window
  left its own real socket connection open forever, since nothing joined or interrupted its reader
  thread's blocking `read` - the real, production-relevant bug this issue's own DoD test caught
  (below). `TerminalPane`'s `Drop` only cancels its output task, `Agents` has none, and `RepoHost`/
  `Hosts` have none either - `Connection::Remote`'s own `RemoteRepoHost::drop` only ends a worker
  thread, a pure disconnect, matching this rule everywhere production ever constructs one.
  `Connection::InProcess` (`#[cfg(test)]`-only, never constructed in production at all) is the one
  deliberate exception: dropping it *does* kill, because a throwaway test host has no other real
  process serving it and GitHub issue #530's own regression
  (`crate::test_support::dropping_a_test_app_leaves_no_spawned_agent_process_behind`) requires
  exactly that - a test app that owns its own in-process host must not leak the real processes it
  spawned once the test ends. `jerry_host::Host`'s own `Drop` (which always kills, via
  `Host::shutdown`) is correct there for the identical reason: an in-process test host has no
  external process to detach from, only its own real children to reap.

**Three real bugs this issue's own DoD test found, past the socket/session-management pieces
already covered above:**

1. **A session's real ConPTY/PTY startup handshake hangs forever with no client attached yet.**
   Windows ConPTY sends a cursor-position query (`ESC[6n`) as part of spawning any real
   interactive shell, and blocks its *entire* output stream until something writes back a real
   reply (`crate::terminal::pane`'s own docs, confirmed live). `crate::terminal::pane::
   TerminalPane`'s own output task already answers this once a client is attached, but nothing
   answered it for the ordinary "spawned, not yet attached" gap every session has - a session
   spawned and reattached to later (this issue's whole point) could die waiting on a query nobody
   ever saw. Fixed in `jerry_host::data_plane::DataPlane::push`, which now reports whether a chunk
   was actually captured (delivered live or buffered for an in-flight attach) or genuinely
   dropped; `crate::session::spawn_relay` answers the query itself, from the host's own headless
   grid, exactly when `push` reports a chunk dropped - the one case in which no client's own grid
   will ever see, and so ever answer, these same bytes, so answering twice is impossible by
   construction.
2. **A freshly spawned agent's `host_session_id` was never persisted before a reasonably-timed
   quit.** `AdeApp::record_worktree_session`'s synchronous call from `Agents::spawn`'s own caller
   necessarily records `host_session_id: None` - the real id is only learned once this session's
   own `SessionSpawn`/attach round trip resolves, asynchronously. Nothing re-persisted afterward
   unless some *other*, unrelated tab mutation happened to trigger it first - a spawn-then-quit
   with no other activity (the ordinary "started a shell, did nothing else, closed the window"
   case) recorded a tab with no id to reattach by at all, so a relaunch's reconciliation always
   fell back to a fresh spawn instead of finding the still-live session. Fixed by
   `crate::work_surface::agents::attach_pane_to_session` re-persisting once its own attach
   succeeds and the real id is known.
3. **`SocketSessionAdapter` had no `Drop` impl**, covered in the bullet above - the socket a
   detaching client leaves open never closes, so the host's own single-attach-at-a-time gate
   (`DataPlane::attached`) never resets, and every later reattach for that session answers
   `session-already-attached` forever. `SocketSessionAdapter::connect`'s reader thread is blocked
   in a plain `read` on its own cloned handle, with no other way to notice the adapter itself is
   gone; a real `UnixStream::shutdown(Shutdown::Both)` on `Self::shutdown_stream` - a *different*
   handle to the same real connection, called from whichever thread drops the value - is what
   actually severs it and unblocks that read, the identical call `Self::shutdown`'s own kill path
   already made.

**A real race in this issue's own DoD test, not a product bug:** `#[cfg(test)]`'s `Connection::
InProcess` means a test app opened against a real repository always starts a throwaway, always-
fresh in-process `jerry_host::Host` for it (`crate::host::connect_repo_host`'s own eager, though
asynchronous, default path from `AdeApp::add_repo`) - a test that wants to *substitute* a specific
connection instead (`AdeApp::adopt_repo_host_for_test`) is racing that default connect for the
same repository, and losing is a real, reproducible failure: the default connect's own throwaway
host can win, spawn the worktree's guaranteed startup shell onto *itself*, and then get replaced
(and, per the bullet above, killed) the instant adoption's own `Hosts::by_repo` insert overwrites
it - `crate::work_surface::session::session_restore_tests::launch_against_adopted_host`'s own docs
cover the fix (build with no repo and `use_remembered_repo: false`, adopt, *then* open the
repository through the adopted connection) and are the one place any future test needing the same
shape should look first, rather than re-deriving it.

**A real, load-bearing bug the async reconciliation introduced, and its fix:** making
`restore_worktree_session` asynchronous (it needs a real `SessionsQuery` round trip) broke an
assumption two independent callers relied on - `AdeApp::select_worktree` and `AdeApp::
spawn_initial_shell_for_opened_repo` both call it for the same just-opened repository's worktree
in the same synchronous continuation, and the second call's idempotency guard
(`session_restored: HashSet<PathBuf>`) answered a fake `Task::ready(())` rather than a task that
actually waited for the first (real) one to finish - harmless when restoration was synchronous,
a real race once it wasn't. `spawn_initial_shell_for_opened_repo`'s own "is this worktree still
empty" guaranteed-shell check then ran before the real reconciliation had landed a single tab,
stacking a redundant guaranteed shell on top of whatever was about to be restored. Fixed by
`select_worktree_maybe_restoring(index, restore: bool, ..)`: the opening continuation calls it
with `restore: false`, so `spawn_initial_shell_for_opened_repo`'s own `restore_worktree_session`
call is the one and only owner of that worktree's reconciliation, start to finish; every other
`select_worktree` call site is unchanged. Caught by the pre-existing `session_restore_tests` fixtures
once they stopped passing after the reconciliation change - not a new test, the existing regression
coverage doing exactly its job.

**Not attempted here:** `crates/jerry-host/tests/real_binary_spawn.rs`'s own
`output_produced_while_no_client_is_attached_is_visible_in_the_snapshot_after_a_fresh_attach` -
this issue's external-tier DoD test against the real, separate `jerry-host` binary - is written
and reviewed but not confirmed passing in every builder's own sandbox; see that test's own commit
message for which real-process constraint blocked verification there. A rarer, second instance of
the double-restore race this entry's own fix above closes remains open: `select_worktree`'s
*ordinary* (non-opening) call to `restore_worktree_session` can still race a *user's* raced click
on the same worktree while `load_worktrees_for_opened_repo`'s own fetch is still in flight - narrower
than the reliably-reproducing case above (it needs a real click during a real async window, not a
deterministic double call), and not covered by a regression test.
