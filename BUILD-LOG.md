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
