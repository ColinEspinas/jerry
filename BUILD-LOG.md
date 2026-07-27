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
