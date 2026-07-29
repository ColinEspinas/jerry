# ade ("Jerry")

A desktop app, built with [GPUI](https://www.gpui.rs/) (Zed's UI framework), for
supervising several AI coding agents (Claude Code, Codex, ...) at once, each running in
its own real git worktree.

The UI is three fixed zones:

- **Session rail** (left) — one row per session (one agent, one worktree, one task),
  with real status derived from the underlying git/process state, not a mocked badge.
- **Work surface** (centre) — a tabbed pane per session with a real spawned shell/agent
  CLI, rendered through genuine `alacritty_terminal` grid emulation (cursor-addressed
  redraw, not a scrolling text dump).
- **Files / changes** (right) — a real recursive file tree, a real diff view against the
  session's detected base branch, real merge-conflict resolution, and a read-only code
  viewer with syntax highlighting and a real `rust-analyzer` LSP client (hover, go-to-
  definition, diagnostics).

There's also a command palette and a settings surface. See [BUILD-LOG.md](BUILD-LOG.md)
for the full, step-by-step history of how each of these was built, and
[ASSESSMENT.md](ASSESSMENT.md) for a candid end-of-build assessment of what's genuinely
solid versus rough. This README does not repeat their content, and tries not to oversell
past what those two documents actually establish.

## Status

This is a working prototype, not a released product. It was built end-to-end (including
this README) largely through AI-agent-driven development sessions on a single Linux/WSL2
machine; see BUILD-LOG.md and ASSESSMENT.md for exactly what has and hasn't been verified,
including known rough edges (for example: the default build only enables GPUI's `wayland`
backend, and has not been screenshotted from that exact default configuration — see
ASSESSMENT.md for why).

## The stack

- **Rust**, pinned to the exact toolchain in [`rust-toolchain.toml`](rust-toolchain.toml)
  (currently `1.95.0`, with `rustfmt`/`clippy` components).
- **[GPUI](https://www.gpui.rs/)** for the UI, pulled in as a path dependency on a local
  checkout of [zed-industries/zed](https://github.com/zed-industries/zed) — see
  [vendor/zed](#vendorzed-required-setup) below, this is **not optional** and the
  workspace will not build without it.
- **[`alacritty_terminal`](https://github.com/zed-industries/alacritty)** (the same fork
  and pinned revision Zed's own `terminal` crate uses) for real ANSI/grid terminal
  emulation, and **`portable-pty`** for spawning and managing the underlying PTY
  processes.
- **[`gix`](https://crates.io/crates/gix)** for read-only git operations (worktree/diff
  enumeration) and the real **`git` CLI** (via explicit argument vectors, never shell
  strings) for anything that mutates the repository (worktree add/remove, merges).
- **[`lsp-types`](https://crates.io/crates/lsp-types)** plus a hand-written client in
  `lsp-core` for real `rust-analyzer` integration (not `vendor/zed`'s own LSP client —
  see `crates/lsp-core/Cargo.toml` for why).
- **`tree-sitter`** / **`tree-sitter-rust`** for real Rust syntax highlighting in the
  code viewer.

## Workspace layout

```
crates/
  wt-core/    git worktree management (gix reads, git-CLI mutations)
  pty-core/   real PTY process spawning, I/O streaming, resize, process-tree teardown
  lsp-core/   real LSP client (spawns and speaks LSP to rust-analyzer)
  app/        the GPUI application itself (binary crate)
vendor/zed/   a local checkout of zed-industries/zed — gitignored, see below
assets/       bundled fonts (IBM Plex Sans/Mono, OFL) used by the app
design_handoff_jerry_ade/
              the design handoff this UI ("Jerry") was built from: a high-fidelity
              HTML mockup + design tokens, kept for reference during UI work — not
              something to port markup from, and not itself part of the app.
```

## Building and running it

### `vendor/zed` (required setup)

`vendor/zed` is **not** a git submodule and is **not** committed to this repository — it's
gitignored outright (see [`.gitignore`](.gitignore)) because it's a full checkout of a
separate, very large upstream Cargo workspace. `crates/app/Cargo.toml` depends on
`gpui_platform` via a path into it, and the root [`Cargo.toml`](Cargo.toml) depends on
`gpui` the same way, so **the workspace will not build until this exists on disk**. Fetch
the exact commit this workspace was built and verified against (also pinned in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) as `ZED_VENDOR_COMMIT`):

```sh
git init vendor/zed
git -C vendor/zed remote add origin https://github.com/zed-industries/zed
git -C vendor/zed fetch --depth 1 origin 7b030b500810b04cf5fb4aa5973be99a502d9f36
git -C vendor/zed checkout FETCH_HEAD
```

A different (e.g. newer) `zed-industries/zed` commit may or may not still build against
this workspace's code — GPUI's API isn't stable across arbitrary upstream revisions. If
you deliberately bump it, expect to fix real compile errors, not just update a hash.

### System dependencies (Linux)

GPUI's default Linux backend here is `wayland` only (see the comment on the
`gpui_platform` dependency in `crates/app/Cargo.toml` for why `x11` isn't enabled by
default). Building it needs real system dev packages — Wayland client headers, xkbcommon,
Vulkan, fontconfig. On Debian/Ubuntu:

```sh
sudo apt-get install -y \
  build-essential clang cmake pkg-config \
  libfontconfig-dev \
  libvulkan1 mesa-vulkan-drivers \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev
```

This list is derived from (a trimmed-down subset of) `vendor/zed/script/linux`'s own
Ubuntu/Debian dependency list — see that script for the authoritative, exhaustive version,
and the comment above the equivalent CI step in `.github/workflows/ci.yml` for what was
trimmed and why. `libasound2-dev`/`libssl-dev`/`libsqlite3-dev`/`libzstd-dev` were dropped
from this list after checking `Cargo.lock`: no `alsa`/`alsa-sys`, `openssl-sys`,
`libsqlite3-sys`/`rusqlite`, or `zstd-sys`/`libz-sys` appears anywhere in this workspace's
resolved dependency tree, so none of them are actually needed to build it. Running the app
itself (not just building it) additionally needs a real
Wayland compositor available (`WAYLAND_DISPLAY` set) — this project's own development
happened under WSLg on Windows, which provides one; a stock X11-only desktop will not open
a window with the default build (again, see `ASSESSMENT.md`).

macOS and Windows are only build-tested in CI (see below) — nobody has run this app on
those platforms yet.

### Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo run -p app
```

`cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check`
must also pass — see [CONTRIBUTING.md](CONTRIBUTING.md) for the full set of checks and
the project's other hard rules.

## Continuous integration

`.github/workflows/ci.yml` runs `cargo build`, `cargo test`, `cargo clippy -D warnings`,
and `cargo fmt --check` on Linux (the platform this project can actually verify end to
end), plus a build-only job on macOS and Windows. See that file's comments for the
`vendor/zed` fetch strategy and the reasoning behind the trimmed system-dependency list.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option, matching the license of GPUI and this
project's other key dependencies (see [BUILD-LOG.md](BUILD-LOG.md) for the stack's
licensing rationale).

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you shall be dual licensed as above, without any additional
terms or conditions.
