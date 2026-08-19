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

**Context:** `wt-core`, `pty-core`, and `lsp-core` were already built with no `gpui` dependency —
verified: the only two occurrences of the string `gpui` in those three crates are comments, one
about a version pin, the other noting that a real GPUI fake-clock test isn't possible without one.
This wasn't written down anywhere, so it was one dependency addition away from silently breaking.

**Decision:** No crate other than `crates/app` (and the planned `crates/jerry-cli`, which must
never gain one either) may depend on `gpui` or `gpui_platform`. This is the foundation the rest of
the target architecture is built on: it's what makes a headless CLI over the same domain logic
possible at all.

**Consequences:** A PR adding `gpui` to `wt-core`/`pty-core`/`lsp-core`'s `Cargo.toml` is a hard
reject, not a design discussion. Any type crossing from a core crate into `crates/app` and back
must be plain data — never a `gpui::Context`, `Window`, or similar (`crates/app/src/work_surface/agents.rs`
violates this today by taking `Context<AdeApp>` directly in agent-lifecycle methods; tracked as
follow-up, not retroactively blessed). `lsp-core`'s one cross-crate dependency (`pty-core`, for
`resolve_on_path`) staying a path dependency between two gpui-free crates is fine and doesn't need
repeating elsewhere.

## 2. Commands and queries, not loose functions, as the application-layer unit

**Status:** Accepted.

**Context:** `wt-core` exposes its capabilities as loose, well-named functions —
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

`wt-core`'s existing functions map onto this almost one-to-one — `commit_all_changes` becomes
`CommitAllChanges { paths: Vec<PathBuf> } -> CommitAllChangesOutcome`, and so on. The
transformation is mechanical: the logic inside each function doesn't change, only its calling
convention does.

**Consequences:** `crates/jerry-cli` becomes possible without duplicating logic — it constructs the
same `Command` values the view does and calls the same `execute`. New capabilities are added as new
`Command`/`Query` types starting now, even though the existing `wt-core` functions aren't
retrofitted in this pass. This is deliberately *not* a command bus with an execution log — undo/redo
and provenance tracking already exist as their own hand-built mechanisms (`wt-core::undo`,
`crates/app/src/provenance/`); layering a generic event-sourced bus on top would duplicate them for
no immediate benefit. A real need for a unified execution log would be its own new entry here, not
an assumption baked into this one.

## 3. The view dispatches commands and queries; it never calls an adapter directly

**Status:** Accepted. Partially enforced — see Consequences.

**Context:** `crates/app`'s render layer currently calls straight into `wt-core` and, in one place,
straight into a raw process spawn: `graph_view/render.rs` alone has 109 `wt_core::` references,
`sidebar/render.rs` has 33, and `sidebar/render.rs:6534` shells out to
`std::process::Command::new("git")` directly, bypassing `wt-core` entirely. Several `render.rs`
files also call `cx.background_spawn`/`cx.spawn` directly around adapter calls, duplicating the
offload-to-background decision ad hoc at every call site.

This works today because `crates/app` is the only consumer of `wt-core`. It becomes a real problem
the moment a second consumer (`crates/jerry-cli`) exists: behavior implemented as "whatever the
render function happens to do around the `wt_core::` call" isn't available to the CLI, and a bug
fixed in the view's copy has to be separately remembered in the CLI's.

**Decision:** Render code (`render.rs`, anything returning `impl IntoElement` or implementing
`Render`) may only read state already held on `AdeApp` and dispatch a `Command`/`Query`, rendering
the outcome. It may never call `wt_core::`, `pty_core::`, `lsp_core::`, or `std::process::Command`
directly. The background-spawn decision moves into the dispatch layer, once, instead of being
repeated at every call site.

**Consequences:** This is currently violated at scale (204 direct adapter references across
`render.rs` files, per `.claude/conventions-baseline.json`) and is **not** retroactively fixed by
this decision — it's the target; the gap is tracked as GitHub issues. New render code must not add
new adapter calls, effective immediately, and this is now **mechanically checked**, not just
reviewed against: `.claude/hooks/check-conventions.sh` greps every `render.rs` file for
`wt_core::`/`pty_core::`/`lsp_core::`/`process::Command::new` and fails — in the pre-commit hook and
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

**Context:** `wt-core` has both `gix` and `std::process::Command` available to it, and the choice
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

**Decision:** `wt-core::worktree_files` is the single answer, built on `git ls-files --cached
--others --exclude-standard`. New callers use it rather than growing a third walk.

**Consequences:** "Content" means what git would show: tracked paths stay listed even under a
later-added ignore rule, and untracked paths appear only if git would offer to stage them. A caller
that must keep working outside a git repository owns its own fallback, since this returns an error
there rather than an empty list.

## 7. Interactive rebase is driven through git's own editor hooks, not reimplemented

**Status:** Accepted.

**Context:** Jerry needs `git rebase --interactive`'s six todo verbs without an interactive
terminal. The alternative to driving real git is reimplementing the todo machinery on plumbing
(`cherry-pick`, `commit --amend`, `reset`) — which means re-deriving conflict handling, `squash`
message combination, `REBASE_HEAD`/`stopped-sha` bookkeeping, and resume-after-restart semantics
from scratch.

**Decision:** Drive the real `git rebase -i` non-interactively through the same environment hooks
a human's `$EDITOR` is invoked through. `GIT_SEQUENCE_EDITOR` is set to a `cp` of a
Jerry-written todo file, replacing git's generated one. `GIT_EDITOR` is a `/bin/sh` script that
classifies each invocation *by the content of the message file git hands it*, never by invocation
order:

1. First line `# This is a combination of ...` → a `squash` combination; accept unmodified.
2. Contains `You are currently editing a commit` → a `reword`; pop the next slot from a persisted
   message queue. Nothing queued means exit non-zero, which reproduces `edit`'s stop exactly.
3. Anything else → a conflict-resumed step; accept git's pre-filled message and **do not** advance
   the queue cursor.

Case 3 is not optional. A conflict-resumed `pick` goes through git's ordinary `commit` codepath and
does open the editor; treating that as a `reword` consumes a message meant for a later row.

**Consequences:** Sidecar state (todo file, editor script, message queue, cursor, and a plan
cross-reference) lives under `<git-dir>/ade-rebase/`, resolved per-worktree rather than in the
shared common dir, and survives until the rebase completes or aborts — so a stop can be
reconstructed after a process restart, including whether a row was `edit` or a message-less
`reword`, which git alone cannot distinguish once stopped. The queue is plain files, not JSON, so
the `sh` script needs no parser. Env var values are spliced unquoted into a shell command line by
git, so every embedded path must be POSIX-single-quoted. The editor script is `/bin/sh`, making
this path Unix-only; elsewhere it surfaces as an ordinary spawn failure. Conflicts are never
auto-resolved or rolled back, matching `crate::rewrite`.

## 8. `pty-core` owns spawning only; `alacritty_terminal` stays in `crates/app`

**Status:** Accepted.

**Context:** Upstream Zed drives `alacritty_terminal::tty::Pty` directly and lets its `EventLoop`
own a thread that both pumps bytes and feeds the `Term` grid parser — one composition, not
separable into a standalone spawn primitive.

**Decision:** `pty-core` owns spawn, raw-byte output, resize and kill via `portable-pty`, and knows
nothing about ANSI escapes or grid state. `crates/app` owns the `Term` grid, driven by the bytes
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
the child is reaped. Callers must therefore poll `try_wait` rather than wait for the output channel
to disconnect. These paths are `#[cfg(windows)]`, never `#[cfg(not(unix))]`, so an unsupported
non-unix target fails to compile instead of silently inheriting Windows semantics.

## 9. `crates/test-support` is a real crate, not a feature-gated one

**Status:** Accepted.

**Context:** Test setup had no shared home, so it was copy-pasted instead: `fn git(dir, args)`
appeared ~30 times across `wt-core` and `app`, alongside 1,223 separate tempdir setups and 303
wall-clock waits. The obvious single crate to fix that has a trap in it — `crates/app`'s fixtures
need `gpui` (a test window, `VisualTestContext`), and `wt-core`/`pty-core`/`lsp-core` must be able
to dev-depend on the same crate without acquiring `gpui` (§1).

A Cargo feature (`test-support = { features = ["gpui"] }` for `app` only) looks like it solves
this and does not: features unify across a workspace build, so one crate enabling `gpui` enables it
for every other crate resolving the same dependency. The core crates' dev graph would silently
regain `gpui` — exactly the outcome §1 exists to prevent, and one no `Cargo.toml` review would
catch.

**Decision:** Two homes, split by dependency rather than by feature. `crates/test-support` is
`gpui`-free and depends only on `tempfile`; anything needing `gpui` lives in
`crates/app/src/test_support.rs`, inside the one crate already allowed to have it.

**Consequences:** `cargo tree -e normal,dev,build -i gpui` reaching only `crates/app` is a
checkable invariant, not a convention. A helper that "just needs a `TestAppContext`" is not added
to `crates/test-support` under any flag — it goes in `crates/app` or it is restructured to take
plain data. The policy those fixtures serve is [`docs/testing.md`](../testing.md).

## 10. Every non-PTY child process is constructed through `pty_core::new_std_command`

**Status:** Accepted.

**Context:** The release binary is a GUI-subsystem process on Windows (`windows_subsystem =
"windows"` in `crates/app/src/main.rs`, adopted from Zed in #451 so no console opens behind the
window). On Windows, a console-subsystem child — `git.exe`, an npm `.cmd` shim, `cmd /c start` —
spawned from a consoleless parent allocates its own *visible* console window unless the spawn
passes `CREATE_NO_WINDOW`. Jerry spawns git continuously from launch (status poll, worktree
watch, Changes refresh), so the missing flag showed up as an endless storm of console popups
(#465). Zed pairs the same attribute with construction-time wrappers
(`util::command`/`gpui_util::new_std_command` upstream) plus a clippy ban on bare constructors;
#451 copied the attribute without the wrapper.

**Decision:** One constructor, `pty_core::new_std_command`, sets `CREATE_NO_WINDOW` on Windows
and is the identity elsewhere; every production `std::process::Command` in the workspace is built
through it. It lives in `pty-core` because that crate already owns "how children are spawned on
this OS" (`resolve_on_path`), and a one-function helper does not earn its own crate. `wt-core`
takes a dependency on `pty-core` for it — the constructor must exist in exactly one place.
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

**Decision:** At the top of `main()`, before anything can spawn, `crates/app`'s `job_object`
module creates one unnamed job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE |
JOB_OBJECT_LIMIT_BREAKAWAY_OK` and assigns *this process* to it. Children join a member's job
automatically at `CreateProcess` time, so every spawn — agent PTYs through ConPTY, every
`new_std_command` child, LSP servers, future call sites — is covered with no per-site plumbing.
The job handle is deliberately never closed: the kernel closes it when the process terminates,
by any means, and then kills every remaining member. Setup failure is logged and non-fatal
(behavior degrades to exactly the destructor-only world this replaces).

It lives in `crates/app`, not `pty-core`, for two reasons: the job is app-lifecycle policy, not
per-session PTY mechanics, and CLAUDE.md pins the core crates as `unsafe`-free — `crates/app`
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
does a real read-modify-write merge (`crates/app/src/hooks/cursor_hooks_file.rs`) rather than
generating the file outright: unparseable JSON aborts the whole operation untouched, every
unrelated key and entry survives byte-for-byte, and Jerry's own entries are identified by a
forwarder script path substring rather than a marker field (the entry shape has no room for one).
The forwarder script itself moves out of the per-launch, `Drop`-deleted temp directory
`crates/app/src/hooks/settings_file.rs` uses for Claude into a stable, version-stamped path under
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
