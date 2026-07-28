# Build Log

## Environment / setup decisions

- Host was Ubuntu 22.04 with rustc 1.81 as the default toolchain, but `vendor/zed/rust-toolchain.toml`
  pins 1.95.0, which GPUI's Cargo.toml (edition 2024, workspace-inherited deps) requires. Installed
  1.95.0 via `rustup` (already present on the box) and pinned this repo's own `rust-toolchain.toml`
  to 1.95.0 as well.
- Verified empirically (scratch project) that a plain path-dependency on `vendor/zed/crates/gpui`
  from a workspace *outside* the zed workspace resolves correctly: Cargo finds gpui's `[workspace]`
  root by walking up gpui's own directory tree (vendor/zed/Cargo.toml), independent of which project
  depends on it. `workspace = true` inherited fields in gpui's Cargo.toml resolve fine. This was a risk
  called out up front (nested/foreign workspace membership) and it turned out to be a non-issue.
- `vendor/zed` contains no usage of the `gix` crate anywhere (it wraps the `git` CLI directly via its
  own `crates/git`). Per the spec we still use gix for reads / git CLI for mutations, but gix API calls
  cannot be verified against vendor/zed — verified instead by reading the downloaded crate source under
  `~/.cargo/registry/src/` and docs.rs-equivalent doc comments in that source.
- Network access: crates.io sparse index and github.com are reachable; this matters because gpui pulls
  ~480 transitive crates including two git dependencies (scap, font-kit).
- Workspace layout: `crates/wt-core`, `crates/pty-core`, `crates/app`, plus `vendor/zed` (gitignored,
  reference only) and `vendor/zed/crates/gpui` pulled in via workspace-level path dependency.

## Step log

### Step 1: `wt-core`

- Built `crates/wt-core`: `list_worktrees(repo_path)` reads via `gix` (open the repo,
  enumerate `Repository::worktrees()` proxies plus the main worktree, resolve HEAD per
  worktree); `add_worktree`/`remove_worktree` shell out to real `git worktree add`/
  `remove` via `std::process::Command` argv (never a shell string). No fallback to
  `git worktree list --porcelain` was needed — gix's worktree/head APIs covered
  everything. gix isn't used anywhere in `vendor/zed`, so its API was verified by reading
  the fetched crate source directly under
  `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/gix-0.68.0/src/` (open,
  `Repository::{main_repo,worktrees,work_dir,head}`, `worktree::Proxy::{base,is_locked,
  lock_reason}`, `Head::{referent_name,try_peel_to_id_in_place}`) plus
  `gix-ref-0.49.1/src/fullname.rs` for `FullName::shorten()`.
- Toolchain wrinkle: `gix 0.68`'s transitive dep `kstring 2.0.4` requires rustc 1.96, one
  above this repo's pinned 1.95.0. Pinned it back with
  `cargo update -p kstring --precise 2.0.2` (reflected in `Cargo.lock`) rather than
  bumping the toolchain further.
- Dirty-worktree-removal semantics (the one hard requirement from the spec): dirty =
  any output from `git status --porcelain --untracked-files=normal` inside the worktree —
  both tracked modifications and untracked files block removal without `force: true`,
  conservatively.

#### Audit round: real bugs found and fixed

A `checker` subagent found nine real issues beyond the passing test suite (the dirty-tree
guarantee itself was sound, but its *reliability* had gaps):

- **Path-resolution mismatch (critical).** The dirty check ran with the worktree path
  resolved against the process's CWD while the actual `git worktree remove` call resolved
  the same (possibly relative) path against `repo_path` — they could silently check
  different directories. Fixed by absolutizing `worktree_path` against `repo_path` once,
  up front, and using that consistently everywhere.
- **Inherited `GIT_DIR`/`GIT_WORK_TREE`/etc. env vars (critical).** These override
  `current_dir()` for git's own resolution, so an inherited `GIT_DIR` could make the
  dirty-check silently operate on the wrong repo and report a dirty worktree as clean.
  Fixed by scrubbing all of `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`,
  `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY` on every git invocation.
- **No `--` option terminator.** A caller-supplied `commit_ish` like `--detach` or a
  `worktree_path` starting with `-` was parsed as a git flag rather than a positional
  argument. Fixed by inserting `--` before the first positional arg in both `add_worktree`
  and `remove_worktree`.
- Unbounded-memory dirty check, no `stdin(Stdio::null())` (askpass/GPG-prompt hang risk),
  bare-repo enumeration hard-failing instead of skipping the (nonexistent) main-worktree
  entry, signal-terminated processes collapsing into a fake `-1` exit code, one bad linked
  worktree aborting the entire `list_worktrees` call instead of surfacing per-entry —
  all fixed; see the doc comments and 14 tests in `crates/wt-core/src/lib.rs` for detail.
  `force: true` now pushes `--force` twice (git requires it twice to remove a *locked*
  worktree; a single `--force` still refuses), and an empty `locked` file (lock with no
  `--reason`) now surfaces as `lock_reason: None` rather than `Some("")`.
- Test count: 6 → 14, including a regression test for the exact relative-path failure
  mode above, a bare-repo case, detached HEAD, and lock-with/without-reason.

### Step 2: `pty-core`

- Built `crates/pty-core` as a spawn/stream/resize/kill primitive over `portable-pty`
  0.9.0 (pinned to match `vendor/zed/Cargo.toml`). No `alacritty_terminal` dependency
  here — see the crate's module doc comment (`crates/pty-core/src/lib.rs`) for the full
  rationale, but in short: `vendor/zed/crates/terminal/` turned out not to use
  `portable_pty` at all (it drives `alacritty_terminal::tty::Pty` directly and lets
  alacritty's own `EventLoop` own reading+parsing together, non-separable). Since
  `pty-core` precedes `app` in the build order and is meant to be a clean primitive,
  ANSI/grid parsing via `alacritty_terminal::Term` is deferred to the `app` crate, driven
  by the raw byte stream `pty-core` exposes.
- Verified the entire `portable-pty` API surface used (`native_pty_system`, `PtySystem`,
  `PtyPair`, `SlavePty::spawn_command`, `MasterPty::{try_clone_reader,take_writer,resize,
  get_size}`, `Child`/`ChildKiller::{kill,wait,try_wait,process_id}`) directly against the
  fetched crate source at
  `~/.cargo/registry/src/index.crates.io-6f17d22bba15001f/portable-pty-0.9.0/src/{lib.rs,
  unix.rs,cmdbuilder.rs}`, since `vendor/zed` doesn't exercise this crate. Confirmed via a
  `finder` subagent that no usage of `portable_pty`'s spawn/master/slave/reader/writer/
  resize APIs exists anywhere in `vendor/zed` (only `portable_pty::ExitStatus` is
  referenced once, in `crates/acp_thread`).
- Output streaming: background thread + `mpsc::Receiver<Vec<u8>>` of raw byte chunks
  (documented choice, see module doc comment).
- Load-bearing detail: the parent's `pair.slave` handle is dropped immediately after
  `spawn_command`, otherwise the reader thread would never see EOF once the child exits
  (the parent's own slave-fd reference would keep the master side from ever reading 0).

#### Audit round: real bugs found and fixed

A `checker` subagent audited the first version of this crate and empirically
demonstrated several real bugs (not just style nits — measured with mutation/RSS tests).
Fixed all of them; the crate's design changed substantially as a result. See the
extensive module doc comment in `crates/pty-core/src/lib.rs` for full rationale on each;
summary:

- **Reader-thread fd leak (real, demonstrated).** The original design assumed dropping
  `PtySession`'s `master` handle would close the pty and unblock the reader thread's
  blocking read. It doesn't: `try_clone_reader()` returns an independently `dup`'d fd,
  so that reference stays open regardless. It only *appeared* to work because
  `portable-pty`'s writer-drop trick (writes `\n`+EOT) gets echoed back to the reader
  when local echo is on — with `stty -echo` (how most real interactive programs run),
  the reader thread and its fd leaked for the process's lifetime. Fixed with a
  `filedescriptor::Pipe` self-pipe: the reader thread `poll()`s `[pty_master_fd,
  shutdown_pipe_fd]` and exits deterministically the moment a shutdown byte is written,
  independent of echo state. Regression test:
  `shutdown_joins_reader_thread_even_with_local_echo_disabled` (spawns `stty -echo; cat`,
  asserts `shutdown()` returns within 5s from a watchdog thread).
- **Only the direct child was killed, not descendants that escape the process group.**
  `portable-pty` makes the child a session/process-group leader (`setsid()` in
  `pre_exec`), so `killpg` reaches ordinary background jobs, but a descendant that calls
  `setsid()` itself (e.g. a daemonizing tool) detaches into its own group and is
  unreachable via the parent's pgid alone. Fixed by snapshotting the full descendant set
  via a breadth-first `/proc/<pid>/task/<pid>/children` walk **before** signaling
  anything (reading it after risks a reparenting race), then signaling both the process
  group and every discovered descendant individually. Regression test:
  `drop_terminates_entire_process_tree_including_escaped_grandchild` (spawns
  `sh -c "setsid sleep 100 & echo GRANDCHILD:$!; exec sleep 300"`, asserts both pids gone
  from `/proc` after drop).
- **Unbounded output channel — measured OOM risk.** The original `mpsc::channel`
  (unbounded) let RSS grow without limit against a fast, undrained producer. Switched to
  `mpsc::sync_channel(256)` (~1MB worst case): a full channel blocks the reader thread's
  `send`, which backpressures its `read`, which fills the kernel pty buffer, which
  blocks the child's `write` — correct terminal semantics. Regression test:
  `output_channel_backpressures_instead_of_growing_unboundedly` (measures this process's
  own `/proc/self/status` VmRSS growth against an undrained `yes` pipe for 500ms,
  asserts growth stays under 20MB).
- **`Drop` used to block the calling thread for up to ~2s** (a fixed poll-then-join
  window on the reader thread, plus `portable-pty`'s own `child.kill()` which internally
  sleeps in up to 5×50ms grace-check iterations) — unacceptable for a crate meant to back
  a GPUI terminal widget on the main thread. Split into `Drop` (fast path: signal-only,
  zero grace, a single non-blocking `try_wait`, and — if not yet reaped — hands the
  child off to a short-lived *detached* background thread that finishes `wait()`-ing on
  it so it doesn't linger as a zombie without blocking `Drop` itself) vs. the new
  `pub fn shutdown(&mut self)` (deterministic: blocks through a bounded grace period,
  `SIGKILL`, `wait()`, and joins both the reader and writer threads — meant to be called
  from a background task). Regression test:
  `shutdown_reaps_child_deterministically_without_lingering_zombie`.
- **Kill-after-reap pid-reuse race.** Added an `exited: Option<ExitStatus>` field on
  `PtySession`, set by `try_wait`/`shutdown`/`Drop` whenever exit is observed; `kill`,
  `shutdown`, and `Drop` all skip signaling once it's `Some`, so a later call can't
  accidentally signal an unrelated, pid-reused process.
- **`write_input` used to block the caller and serialize all writers behind a
  `Mutex`.** Replaced with a dedicated background writer thread fed over an
  `mpsc::Sender`; `write_input` now just enqueues (effectively non-blocking for
  realistic input volumes) and the actual, possibly-blocking pty write syscall only ever
  happens on that thread.
- **Silent `cwd` fallback to `$HOME`.** `CommandBuilder::as_command` falls back to the
  user's home directory both when no cwd is set *and* when a set cwd fails `is_dir()` —
  both undocumented. `spawn()` now defaults to `std::env::current_dir()` when
  `SpawnOptions::cwd` is unset, and validates any caller-supplied cwd is an existing
  directory before ever handing it to `CommandBuilder`, returning a typed
  `PtyError::InvalidCwd` otherwise. Regression test:
  `spawn_rejects_nonexistent_cwd_instead_of_silently_falling_back_to_home`.
- **Error type no longer wraps `anyhow`.** Confirmed against
  `anyhow-1.0.104/src/error.rs` that `anyhow::Error` does not itself implement
  `std::error::Error` (only `Display`/`Debug`/`Deref<Target = dyn StdError>`), so
  `#[source] anyhow::Error` would not even compile with thiserror. `PtyError`'s variants
  now carry owned `String`s (via `.to_string()` on the underlying `anyhow::Error`'s
  `Display` output) or concrete `std::io::Error`/`PathBuf`, so the public error type has
  no `anyhow` dependency at all.
- New dependency: `nix` (0.28, `signal` feature only — `kill`/`killpg`/`Pid`, all safe
  wrappers) and `filedescriptor` (0.8 — `Pipe`, `poll`, `pollfd`, `POLLIN`, all safe
  wrappers), both `[target.'cfg(unix)'.dependencies]` and already transitive deps of
  `portable-pty` 0.9.0 on unix (so this adds no new version surface, just promotes
  already-resolved crates to direct deps). Both fully verified against their fetched
  sources at `~/.cargo/registry/src/index.crates.io-6f17d22bba15001f/{nix-0.28.0,
  filedescriptor-0.8.3}` — no `unsafe` needed anywhere in `pty-core`'s own code; a
  `#[cfg(not(unix))] compile_error!` documents that Windows support is an explicit,
  out-of-scope cut for this step (this repo targets Linux/WSL2).
- Tests (`cargo test -p pty-core`, 10 passing, no `.unwrap()`/`.expect()` outside
  `#[cfg(test)]`, no `unsafe` anywhere): short-process output read back correctly; input
  echoed back through pty line discipline; resize on a live session verified via
  `get_size()`; typed-error paths for a nonexistent program and a nonexistent cwd; the
  leaf-process orphan-check (spawns `sleep 100`, drops, polls `/proc/<pid>` for up to
  5s); the process-tree orphan-check with an escaped grandchild described above; the
  `shutdown()` determinism and echo-disabled-reader-join regressions described above; and
  the output-channel-backpressure RSS regression described above. Ran the full suite 5x
  in default (parallel) mode and 8x with `--test-threads=1` — no flakes; suite runtime
  dropped from ~5s to ~0.5-1s in the process (the old `cat`-based echo test used to burn
  its full timeout every run regardless of pass/fail; it now returns as soon as the
  expected text appears).
- `cargo build -p pty-core`, `cargo build --workspace`, `cargo fmt -p pty-core -- --check`,
  and `cargo clippy -p pty-core --all-targets -- -D warnings` all clean.

### Step 3: `app` (the risky one)

- Built a real GPUI three-pane window: `Application`/`cx.open_window`/`cx.new` entry point
  (verified against `vendor/zed/crates/gpui/examples/{hello_world,window}.rs`),
  `Entity<T: Render>`/`Context<T>`/`WeakEntity<T>` state model, structurally patterned
  after `vendor/zed/crates/project_panel`'s sidebar-list-plus-selection shape and
  `vendor/zed/crates/terminal_view/src/terminal_view.rs`'s key-input wiring. Left = real
  `wt_core::list_worktrees` results; center = a real spawned shell via `pty_core`; right =
  a real recursive `std::fs::read_dir` walk of the selected worktree. All three are
  dispatched off the GPUI foreground thread via `cx.background_executor().spawn(...)`
  (gpui's real thread-pool executor, not a stub), updating entity state back on the
  foreground thread inside `this.update(cx, ...)`.
- **Workspace gotcha, non-obvious:** once this repo's own root `Cargo.toml` declared its
  own `[workspace]` table, Cargo started treating `vendor/zed`'s nested crates as
  *implicit members of this workspace* (not just an external path dependency, as the
  earlier scratch-project test showed) — `gpui`'s `foo.workspace = true` fields then
  failed to resolve since our workspace doesn't define those keys. Fixed with
  `[workspace] exclude = ["vendor/zed"]`. Reproduced the failure by temporarily removing
  it during the audit round; restored and confirmed clean.
- Terminal rendering: deliberately does **not** pull in `alacritty_terminal` (matching
  `pty-core`'s own step-2 decision) — a hand-rolled `TerminalBuffer` in
  `crates/app/src/ansi.rs` strips CSI/OSC escapes and renders plain scrolling monospace
  text rather than a full color/attribute grid. This is a real fidelity cut, documented
  in code, not a fake terminal — the bytes are always real.
- Environment finding: root-window screenshots (`mss`, `xwd -root`) come back solid black
  in this sandbox — a WSLg/rootless-Xwayland limitation unrelated to the app (confirmed
  with a plain Tk control window). Per-window capture (`xwd -id <id>`) works. Separately,
  the *default* `wayland`-only build (the one `cargo build -p app` actually produces)
  creates no discoverable X11 window at all in this sandbox, so it could not be
  screenshotted honestly; the `x11` gpui_platform feature link-fails here without
  system packages we don't have passwordless-sudo to install
  (`libxkbcommon-x11`/`libxcb-xkb` missing). Verification screenshots were taken from a
  temporary, local-only x11-patched build (extracted `.deb`s + `LD_LIBRARY_PATH`, never
  touching the system or committed to the repo); `Cargo.toml` was confirmed reverted to
  the wayland-only default afterward (`ldd target/debug/app` shows no
  `libxkbcommon-x11`). This means step 3's visual proof is real but was not captured from
  the exact binary a plain `cargo build -p app` produces on *this* machine — it would be
  on a machine with `x11` system libs present, or already is on the shipped `wayland`
  path against a real (non-sandboxed) Wayland compositor.

#### Audit round: real bugs found and fixed

A `checker` subagent found one serious functional bug and several robustness issues in
the first version, beyond the passing test suite and a plausible-looking screenshot:

- **Headline bug: the center pane rendered no actual output, only the trailing prompt
  (critical, empirically proven).** A PTY has `ONLCR` on, so every real `\n` a child
  writes arrives as `\r\n`. The original `TerminalBuffer` treated `\r` as "clear the
  current line immediately" — so the CR right before each LF wiped the line's text before
  the LF could commit it. `append_bytes(b"HELLO\r\n")` rendered `["", ""]`. The only reason
  the first screenshot looked alive was that a shell prompt has no trailing newline, so it
  survived as the in-progress `current_line`; every actual line of command output was
  invisible. This is exactly the kind of bug a "no fake functionality" bar exists to catch
  even when nothing was faked on purpose — real bytes were flowing in and being silently
  discarded on the way to the screen. Fixed with a deferred `pending_cr` flag: `\r` no
  longer clears eagerly; it only clears (as a real column-0 overwrite, for `\r`-only
  progress-bar-style updates) if the *next* byte isn't `\n`. Verified after the fix with a
  screenshot of `printf 'LINE-ONE\nLINE-TWO\nLINE-THREE\n'` typed into the live terminal
  pane, rendering as three separate correct lines followed by a fresh prompt.
- **No keyboard input path at all (functional gap, not just a fidelity cut).** Nothing in
  the first version called `pty_core`'s write-input or resize APIs — a spawned interactive
  shell just sat at its prompt forever with no way to type into it. Fixed: the terminal
  pane now takes focus (`FocusHandle`/`Focusable`/`.track_focus`), handles
  `.on_key_down`, maps printable chars / Enter / Backspace / Tab / Escape / arrows / Ctrl+letter
  to real bytes via `PtySession::write_input`, and issues an approximate resize from the
  window's viewport size on render.
- Unbounded `current_line` growth (no per-line cap, unlike the already-bounded completed-
  line deque) — capped at 8KiB with forced commit past the cap. Unbounded per-tick ANSI
  decode work on the GPUI foreground thread (a firehose child could hand it ~1MB of
  byte-by-byte decode work every ~33ms poll tick) — capped at 64 chunks drained per tick.
  `scroll_to_bottom()` was called unconditionally inside `render()` using stale prior-
  frame measurements — moved into the poll loop, only invoked when new output actually
  arrived.
- The right-sidebar file tree was also a real bug source during initial verification, not
  just an audit finding: an early version rendered every *loaded* file-tree entry
  (uncapped) as a real `div()`; against this repo's own `vendor/zed` subtree (~5000
  entries), re-laying that out via Taffy on every ~33ms poll tick pegged the foreground
  executor badly enough that unrelated timers fired 10+ seconds late, and the UI looked
  frozen/stale even though background loads and `cx.notify()` were both firing correctly.
  Root-caused with a minimal counter-entity probe that confirmed GPUI's own spawn/notify/
  redraw pipeline was fine, which pointed the search at application code. Fixed with a
  separate, smaller *rendered*-row cap (500) independent of the *loaded* cap (5000);
  documented in `root.rs`. This is the kind of thing "commit after each step" and "checker
  after each step" both exist to catch before it compounds into steps 4/5.
- Test count: 43 → 49 workspace-wide after the fixes (25 in `app` alone), including an
  end-to-end regression that spawns a real `printf` through `pty_core::spawn` and feeds
  its actual output through `TerminalBuffer` — the shape of test that would have caught
  the CR/LF bug the first time.

### Step 4: Sessions, and correcting a real stack deviation

- The immediate goal was "spawn an agent CLI (Claude Code / Codex) in a chosen worktree,
  with its output in the center pane" — but before building that, the human reviewer
  flagged that step 3's `ansi.rs` (a hand-rolled CR/LF-scanning plain-text buffer) was
  never actually replaced with real `alacritty_terminal` grid emulation, despite the
  stack being fixed to "alacritty_terminal plus portable-pty for terminals." This was a
  real, correct catch: an interactive agent CLI is exactly the case a plain-text scanner
  can't handle (cursor-addressed redraws render as garbled repeated lines, not clean
  updates), which would have made Sessions look superficially done while being broken for
  its actual purpose. Fixed first, before building sessions/tabs on top of it.
- Deleted `ansi.rs` entirely. New `crates/app/src/terminal_grid.rs`: `TerminalGrid` wraps
  a real `alacritty_terminal::Term<NoopEventListener>`, fed via
  `alacritty_terminal::vte::ansi::Processor::advance` (alacritty re-exports `vte` itself,
  confirmed via `pub use vte;` in its `lib.rs` — so no separate top-level `vte` dependency,
  avoiding version drift). `alacritty_terminal` pinned to the exact same git rev
  `vendor/zed/Cargo.toml` uses (`4c129667ce56611becdc82de6e28218c80e2e88f`), specifically
  *because* the reference implementations this was verified against —
  `vendor/zed/crates/terminal/src/terminal.rs` and
  `vendor/zed/crates/terminal_view/src/terminal_element.rs` — were themselves written
  against that exact fork/rev; using upstream crates.io `alacritty_terminal` instead would
  have broken the "verify against vendor/zed" methodology this whole project relies on,
  since the API surfaces aren't guaranteed to match.
- Colors: fixed ANSI-16 palette + standard xterm 256-color cube/grayscale-ramp formulas
  (matching Zed's own `get_color_at_index`); verified independently by an audit round
  against the known xterm math (cube stops 0,95,135,175,215,255; grayscale 8..238 step 10).
- New `crates/app/src/sessions.rs`: `SessionKind::{Shell,Claude,Codex}` each map to a real
  `TerminalSpec`; `Sessions` owns open sessions and which is active. Closing a tab calls
  `TerminalPane::shutdown()` → `PtySession::shutdown()` on the background executor —
  deterministic, confirmed-reaped teardown, not just dropping the entity and hoping
  `Drop`'s fire-and-forget signal gets there eventually.
- Deliberate behavior change from step 3, documented in `root.rs`'s module docs: selecting
  a worktree in the sidebar no longer respawns the active terminal. It only changes where
  *new* sessions spawn. Step 3's respawn-on-select behavior would mean clicking a worktree
  to browse its files could silently kill a live agent session — unacceptable once
  sessions are meant to be long-running and valuable.
- Real proof: a live interactive `claude` session (the actual `claude` binary installed
  on this dev machine, `2.1.220`) rendering its box-drawing welcome screen correctly
  through the new grid pipeline — cursor-addressed redraw-in-place, not garbled repeated
  lines. A "New Codex Session" attempt fails with a real, non-panicking, in-tab error
  (`codex` genuinely isn't installed here) rather than a simulated one. Automated
  tests/screenshots use `claude --version`/`--help` (fast, deterministic, no API cost)
  rather than a full interactive agentic session, to avoid incurring real API usage during
  verification.
- Audit round found the integration itself solid (every `alacritty_terminal`/`vte` API
  claim checked out against the real pinned source; 41/41 tests independently
  reproduced; resource bounds — 256KB/tick decode cap, 500-row file-tree render cap,
  alacritty's own bounded scrollback — all held) and one minor issue: `TerminalPane::
  respawn` had become dead code once worktree-selection stopped respawning anything.
  Removed rather than left as unused public API.
- Infra note, not code: partway through this step, the background builder agent was
  killed mid-edit by what looked like an external stop (later confirmed a mis-click), and
  shortly after, a session reconnect silently dropped the custom `builder`/`checker`/
  `finder` subagent definitions entirely (they'd been defined via `claude --agents
  '{...}'` at process launch, not via `.claude/agents/*.md` files, so nothing on disk
  survived the reconnect). Recreated them as `.claude/agents/{builder,checker,finder}.md`
  once the user supplied the original launch command, matching the original prompts and
  per-agent models (`builder`→sonnet, `checker`→opus, `finder`→haiku) exactly, and
  committed them so a future reconnect doesn't lose them again.

### Step 5: read-only diffs, and the last step

- "Base" decision: a git worktree has no explicit "base branch" concept, so `wt_core::
  diff::diff_against_base` detects the repository's default branch in order —
  `refs/remotes/origin/HEAD` (symbolic ref) → local `main` → local `master` → the main
  worktree's own currently-checked-out branch — then computes the merge-base with the
  selected worktree's `HEAD` via `gix`. If the worktree's branch *is* the detected default,
  or no merge-base exists (unrelated histories), that's a real, rendered, non-error UI
  state (`DiffBase::OnDefaultBranch`/`NoBaseFound`), not a fabricated empty diff.
- Diff content: `git diff <merge-base>` (not `<merge-base>..HEAD`) — deliberately, so
  uncommitted working-tree changes render alongside committed-since-branch-point changes,
  since an agent's work may not be committed yet and that's exactly what a reviewer needs
  to see.
- gix-vs-git-CLI split, mirroring wt-core's own established pattern: base detection and
  merge-base computation go through `gix` (a real, stable API for both). Producing the
  actual diff *text* shells out to `git diff` instead — reimplementing `git diff`'s unified
  diff formatter (hunk headers, rename detection, binary detection, working-tree-aware
  content) on top of `gix-diff`'s lower-level tree/blob primitives was judged a real risk of
  subtly-wrong output for a lot of effort, versus `git diff` itself being definitionally
  correct.
- First audit round found the design/API-verification work solid (every gix/git-CLI claim
  checked out) but three critical, empirically-demonstrated bugs and two lesser ones in the
  diff-parsing and process-handling logic — the kind of thing that's easy to miss by
  reading code but shows up immediately under adversarial testing:
  - **Hunk-body lines starting with `--- `/`+++ ` were misparsed as file headers** (e.g. a
    deleted SQL comment line `-- foo` renders as `--- foo`), silently truncating the rest
    of that hunk and, worse, misattributing content to the wrong filename entirely — for a
    tool whose purpose is reviewing an agent's changes before merge, showing a change under
    the wrong file's name is close to the worst failure mode available. Fixed by tracking
    hunk boundaries from the `@@ -a,b +c,d @@` header's own declared line counts instead of
    guessing from line prefixes, so header-detection only ever runs outside a hunk body.
  - **Untracked (newly-created, unstaged) files were completely invisible in the diff** —
    `git diff <merge-base>` alone never includes untracked paths, contradicting both the
    module's own docs and the fact that a new file is one of the most common things an
    agent produces. Fixed with a throwaway `--intent-to-add`-augmented shadow index (a temp
    copy of the real index, swapped in via `GIT_INDEX_FILE`) — the real index is only ever
    *read*, never written; verified empirically (a `git status` immediately after diffing
    still reports the file as untracked, never staged).
  - **Path parsing assumed `a/`/`b/` prefixes were guaranteed**, but `diff.mnemonicPrefix`
    (a real, fairly common user git config) changes them to `i/`/`w/`/`c/`, silently
    mislabeling every file. Fixed by pinning `-c diff.mnemonicPrefix=false -c
    diff.noprefix=false -c core.quotePath=false` on the `git diff` invocation itself, rather
    than assuming defaults.
  - Unbounded stderr read plus stdout/stderr drained sequentially rather than concurrently
    — a narrow but real deadlock/child-leak path if `git diff` wrote enough stderr (e.g. one
    warning line per file, across up to 300 files) to fill its pipe buffer before stdout
    finished. Fixed with a capped, concurrently-drained stderr reader.
  - Clicking the Files/Diff toggle didn't recompute the diff, so it could silently go
    stale relative to new agent activity. Fixed by reloading on toggle.
- Test count: wt-core 28 → 36 after these fixes, including regression tests built directly
  from each bug's real reproduction (a deleted `-- comment` line, an added `++ b/evil.txt`
  line, an untracked file appearing as a real addition, `diff.mnemonicPrefix=true` set in
  the test repo, a stderr-heavy child process bounded by a timeout so a regression fails
  fast instead of hanging).
- Infra note: this step's builder agent was interrupted mid-task by another background
  session reconnect (see step 4's note on the same failure mode) partway through — but it
  had already produced 903 lines of genuinely complete, well-reasoned `diff.rs` (the base-
  detection algorithm, the gix/git-CLI split, and the caps were all already correct) before
  being cut off; a fresh `builder` agent picked it up, wired it into the crate, and finished
  the `app`-side UI rather than restarting from scratch.

## Where the original five-step build left the project

All five build steps are complete, checked (each went through at least one adversarial
audit round that found and got real bugs fixed, not just a read-through), and committed.
See `ASSESSMENT.md` for an honest account of what actually runs end to end versus what's
still rough, and where the real effort went as of that point.

## Redesign: "Jerry"

After the five-step build, the user tested the running app and reported six real bugs
(no window controls, laggy terminal, missing file-tree icons + broken scroll/collapse,
terminal scaling/display bugs, non-responsive layout, no pane-resize controls), and
separately provided a full high-fidelity design handoff (`design_handoff_jerry_ade/`) for
a redesign codenamed "Jerry" — exact colors/spacing/type via a ready-to-adapt `tokens.rs`,
and an authoritative interactive HTML mockup. Asked how much of it to build, the user chose
the most ambitious option: everything, fully real, no stubs — including subsystems the app
had no backing for at all (a command palette, a settings surface, real merge-conflict
resolution, a real code editor with a real LSP client). Sequenced into 8 phases; each goes
through the same build → adversarial audit → fix → verify cycle as the original five steps.

### Phase A — foundation: tokens, real window chrome, responsive layout

Ported `tokens.rs` as a real Rust module (`theme.rs`), unchanged values, so later phases
reference `theme::status::ASK` etc. exactly as documented. Bundled real IBM Plex Sans/Mono
(OFL, from IBM's official releases) and registered them with GPUI's real font-loading API.
Built a real custom title bar (GPUI has no reliable native Linux window-manager chrome to
lean on) with working close/maximize/minimize, following `vendor/zed/crates/platform_title_bar`'s
pattern — directly fixes the "no window controls" bug. Root-caused and fixed the
"typing in the terminal pushes the file tree off-screen" bug: GPUI/Taffy flex items default
to a content-derived minimum width, so the terminal's unbounded-width text was forcing the
centre pane wider than its flex share; fixed with `overflow_hidden()`/`min_w_0()` on the
correct flex node.

Audit found two real bugs in the same resize code the layout fix touched: the child pty
was never actually resized past its spawn-time constants (a size computed before the
async-spawned session existed was silently dropped instead of retried), and pty column
count was computed from the whole window's viewport rather than the pane's own real bounds,
silently truncating a large fraction of every line off-screen. Both fixed.

### Phase B — session rail

Rebuilt the left sidebar as the design's session rail: real status derivation
(Ask/Fail/Review/Run/Idle) from real signals — exit code drives Fail/Review exactly, a
documented idle-time heuristic drives Run/Ask (never for a plain shell) — grouping by
urgency or by project/worktree (worktrees without a session shown too, with real
clean/prunable state from a new real `gix` merge-base check), a functional filter, and a
footer prune action.

Audit found a genuinely dangerous bug: prune could delete a worktree with a live agent
session still running in it (a clean tree isn't a dirty tree, so the existing dirty-check
doesn't catch this), and it was a single unconfirmed click that could destroy multiple
worktrees' gitignored-but-real state at once. Fixed with an explicit live-session exclusion
computed before anything is offered for removal, and a real two-click confirmation that
touches nothing on disk until confirmed — the same level of care step 1's dirty-tree
refusal got, applied to a new destructive UI path. Also fixed: an exit-status race that
could misclassify a failed process as Idle forever, ⌘N being swallowed as literal input by
a focused terminal, and a Phase-A-era regression (worktree list errors/locks silently
stopped being surfaced).

### Phase C — work surface restyle + terminal perf

Restyled the tab strip (agent-tinted chip per session kind, real terminal-pane glyph),
session context bar, and CLI/terminal pane chrome. Archive and Interrupt are real; Merge
and the review-status actions have no backing logic yet, so they render genuinely disabled
rather than fake-clickable.

Fixed the reported "terminal scales weirdly" bug for real: pty grid size was computed from
guessed cell-size constants never measured against the actual bundled font, and separately
from the padding box of its measuring canvas rather than the real content area — both
caused requesting more rows/columns than the pane could show. Investigated the reported
"laggy" bug: a first pass misattributed it to GPUI re-rendering the whole app tree on every
terminal poll tick (real, but only ~2ms of frame cost) — an audit round measured GPUI's
actual draw+present time directly and found it eating ~90ms/frame, then a follow-up fix
round traced that to this sandbox having no Vulkan loader, so GPUI's wgpu backend falls
back to a GL-over-D3D12 translation layer that's slow at painting even a modest UI here.
Real, evidenced, environment/GPU-backend finding, not an app bug — documented with real
frame-latency numbers rather than the wrong-but-plausible first explanation.

Also fixed: a residual padding-box-vs-content-box mismeasurement left over from the scaling
fix, and disabled buttons that kept their full color fill (only text/border dimmed) so an
inert button could look more clickable than a real one next to it — exactly what the
no-fake-functionality rule exists to prevent.

### Phase D — files/changes zone

Real GPUI-composed icons (two-rect folder, per-extension language chip) replacing emoji
that rendered as tofu boxes on this machine. Fixed two more reported bugs for real: file
tree scroll was silently clipping content instead of scrolling (`size_full()` inside a
scrollable wrapper pins height to the viewport, leaving no content height to scroll
against), and directories had no collapse state at all. Added real drag-to-resize
splitters between the three zones (fixes "we need sizing control for the panes"), a real
Changes list (review checkboxes, progress bar, diff folding with a real unchanged-line
count from actual hunk ranges), and real cross-worktree state resets.

Audit found real per-frame performance bugs (an O(rendered rows × diff files) scan with a
heap allocation per row, every frame; a full diff re-walk every render regardless of which
tab was showing) and a cross-worktree state-bleed bug (review-checked state and the open
diff weren't reset when switching worktrees, so a file could show as "reviewed" in the
wrong worktree). Fixed.

**Infra incident, not code:** partway through this phase, a background session reconnect
(the same failure mode noted in the original build) dropped a checker agent mid-audit —
but unlike earlier reconnects, this time the underlying process survived independently,
invisible to normal resume/tracking, and kept editing the working tree concurrently with
the freshly-relaunched replacement agent for over an hour before being noticed. It left a
temporary x11-workaround feature flag enabled in `Cargo.toml` (caught and reverted) and,
separately, appears to have committed its own overlapping round of fixes directly —
despite every agent in this project being explicitly instructed not to commit — before it
was found (via the checker's own incident report) and killed. Verified the resulting
commit's content directly (grepped for every fix, independently re-ran the full
build/test/clippy/fmt suite against it) before accepting it rather than reverting real,
working, tested code over a process hygiene violation — but this is a real gap: nothing in
this project's process prevents a rogue background process from committing unreviewed
work, and it happened. Worth remembering if resuming this kind of long multi-agent session
after any interruption: check `git log` for unexpected commits and `ps aux` for orphaned
`--fork-session --resume` processes before trusting the working tree's state.

### Phase E — command palette (⌘K)

Real overlay palette over real data: sessions from `Sessions`, files from the loaded file
tree (changed files surfaced first on an empty query), and a fixed set of commands each
dispatching to an already-real app method. Prune goes through the exact same two-click
code path the rail's own prune button uses, not a shortcut around it — verified by tracing
the call chain to the same function, then executing it live via a real test harness (arm,
confirm nothing's deleted yet, confirm on second run, worktree actually gone).

The infra incident from Phase D got much worse during this phase: the same rogue process
respawned three separate times in real time while dispatching and re-dispatching this
phase, at one point actively interleaving edits with a freshly-launched legitimate agent
in the same files while both were live simultaneously — confirmed via file-modification
timestamps seconds apart. Killing individual process instances stopped working as a fix
(it kept respawning within minutes); traced it to a daemon (`bg-pty-host`) retrying a
specific stale session id (`c500d122`) via `--resume`, and broke the loop at its root by
renaming that session's own transcript file so `--resume` has nothing to find. Held for
the rest of the session after that. Two prior interrupted-session incidents this same
session id caused are also almost certainly attributable to this same daemon retry
behavior, not independent events.

Audit found the palette's own entry point was broken: closing it never restored window
focus to whatever was focused before, so ⌘K worked exactly once per manual click and did
nothing at all on a fresh window before any click had happened — invisible to the builder
precisely because it couldn't test interactively (the same X11 synthetic-input limitation
documented since steps 3-4). This phase's checker worked around that by building a real
GPUI `TestAppContext`/`VisualTestContext` harness to drive actual keystrokes/clicks against
the real app instead of relying on screenshots — reused for the fix's own verification
(the fix agent proved causation by reverting its own fix and watching the new tests fail
with exactly the reported symptoms, then restoring it), and kept as this crate's first
permanent test-support harness, since a `FocusHandle` pointing at an unrendered node is
structurally invisible to plain unit tests. Also fixed: the panel always rendering at max
height regardless of content, and palette actions not clearing an armed-but-unconfirmed
rail prune.

### Phase F — settings surface

A real settings surface (not a modal), following the design doc's own explicit scope
statement: "Agents and Worktrees are designed; the rest are nav-only in this mockup."
Agents page does real `$PATH` detection (Claude genuinely resolves on this machine, Codex
genuinely doesn't); Worktrees page reuses Phase B's real worktree/prune state rather than
a parallel implementation. Every other nav page renders the mockup's own literal
placeholder copy rather than invented settings content that was never designed.

Audit found the Agents page recomputing its PATH search directly in `render()` — fast
(~5µs) when a binary is found, but ~30ms when not found (only the not-found path walks the
whole `$PATH` list), paid every frame and re-triggered by the existing 3-second status
poll while the page stayed open. Fixed with the same background-cache pattern already used
for disk usage. Also found a real, if latent, `unsafe` env-var mutation in pty-core's own
tests whose safety comment incorrectly claimed single-threaded execution — refactored the
PATH search into a pure function taking the PATH value as a parameter instead of reading
process environment internally, removing pty-core's last `unsafe` block entirely rather
than just patching the comment.

The rogue-process fix from Phase E held for the rest of this phase and its audit — no
further sightings.

### Phase G — real git merge and conflict resolution

The first feature anywhere in this project that can create real merge commits or otherwise
alter real git history. Wires up the context bar's "Merge" button (real-but-disabled since
Phase C) to a real `git merge --no-commit --no-ff`, run in whichever worktree already has
the base branch checked out (a real git-worktree subtlety: you can't check out a branch
that's already checked out elsewhere, so the merge has to run from wherever the base
actually lives, never via a fresh checkout). Never auto-commits — both a clean merge and a
resolved conflict require an explicit "Complete merge" action. Real conflict-marker
parsing, real take-left/right/both writing real resolved content, real `git merge --abort`
recovery.

Given the stakes, this got three rounds of adversarial testing against real git repos
before landing, each finding real bugs the previous round missed:

- **Round one** (7 bugs): a missing `core.quotePath` pin meant a non-ASCII filename could
  get a stray, wrongly-named file written into the repo; modify/delete and binary
  conflicts have no textual markers on disk and were silently reported as "resolved,"
  which would let a user click Complete and get a confusing failure instead of a clear
  state; the error state had no abort action, so any read failure after a successful `git
  merge` permanently wedged the base worktree mid-merge with no in-app recovery; merge
  state leaked across three different session-close paths (archive, respawn, tab close),
  only one of which was originally checked, permanently disabling Merge app-wide after any
  of them; Complete/Abort had no in-flight guard; "Take both" had no UI control despite
  being tested at the wt-core level; `complete_merge` had no defense-in-depth precondition
  of its own, relying entirely on git's own rejection.
- **Round two**: the round-one fix introduced its own wedge. Two async operations (a
  session-close cleanup abort, and an in-flight Complete/Abort) shared single task-handle
  fields, and GPUI cancels a `Task` immediately on drop — so closing a session while a
  Complete was landing silently cancelled it via the cleanup abort, discarding real
  resolved work and leaving `merge_op_in_flight` stuck `true` with both recovery buttons
  reduced to no-ops. A sibling bug let resolving two files back-to-back cancel one file's
  write the same way. Both fixed by giving each async operation its own task slot instead
  of sharing one, and having cleanup back off entirely when a real operation is already in
  flight rather than racing it.
- **Round three**: re-verified everything against real repos, including deliberately
  reverting each fix in isolation to confirm its specific regression test actually catches
  the regression, not just that the test suite stays green.

The conflict banner (design's cross-session "another session touched this file" warning)
was scoped out — it needs a per-session parallel diff cache the app doesn't have yet,
documented as a materially separate feature rather than faked.
