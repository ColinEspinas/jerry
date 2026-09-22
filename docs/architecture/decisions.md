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
the child is reaped. Callers must therefore poll `try_wait` rather than wait for the output channel
to disconnect. These paths are `#[cfg(windows)]`, never `#[cfg(not(unix))]`, so an unsupported
non-unix target fails to compile instead of silently inheriting Windows semantics.

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
