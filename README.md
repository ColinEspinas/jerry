# ade ("Jerry")

A desktop app, built with [GPUI](https://www.gpui.rs/) (Zed's UI framework), for supervising
several AI coding agents (Claude Code, Codex, ...) at once, each running in its own git worktree.

The UI is three fixed zones:

- **Session rail** (left) — one row per session (one agent, one worktree, one task), with status
  derived from the underlying git/process state.
- **Work surface** (centre) — a tabbed pane per session with a spawned shell/agent CLI, rendered
  through `alacritty_terminal` grid emulation (cursor-addressed redraw, not a scrolling text dump).
- **Files / changes** (right) — a recursive file tree, a diff view against the session's detected
  base branch, and a code editor with full text editing (cursor/selection/IME support), backed by
  an LSP client (hover, go-to-definition, diagnostics, completions) across several languages, not
  just Rust. Merge conflicts resolve either structurally (per-hunk accept/reject) or by
  hand-editing conflict markers directly, in the same editor.

There's also a command palette (with a "History" group for undoing/redoing worktree-level actions
like committing or discarding a session's changes) and a settings surface (Keybindings, Language
Servers, Themes).

## Status

A working prototype, not a released product — see [open issues](../../issues) for what's rough or
in progress. GPUI's Linux backend compiles both the `wayland` and `x11` features and picks between
them at runtime from `$WAYLAND_DISPLAY`/`$DISPLAY`; macOS has been run locally on Apple Silicon;
Windows is currently build-verified in CI only.

## Quick start

**Prebuilt binary** — grab `jerry-linux.tar.gz`, `jerry-macos.tar.gz`, or `jerry-windows.zip` from
the [latest release](../../releases/latest).

**From source:**

```sh
cargo build --workspace
cargo run --release -p app
```

Needs the Rust toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml) and, on Linux, real
system packages — see [System dependencies](#system-dependencies) below if the plain build fails.
Use `--release`: a debug-profile GPUI build is commonly 5–20x slower for this app's own per-frame
work (layout/paint, `tree-sitter` parsing, terminal-grid decode).

## Architecture

```
crates/
  wt-core/    git worktree management (gix reads, git-CLI mutations)
  pty-core/   PTY process spawning, I/O streaming, resize, process-tree teardown
  lsp-core/   LSP client (spawns and speaks LSP to rust-analyzer and other servers)
  app/        the GPUI application itself (binary crate)
assets/       bundled fonts (IBM Plex Sans/Mono, OFL) used by the app
design_handoff_jerry_ade/
              the design handoff this UI ("Jerry") was built from — reference only
docs/
  architecture/  target architecture, and the reasoning behind each rule
  themes.md, theme-palette-design.md
```

`wt-core`/`pty-core`/`lsp-core` have no `gpui` dependency and never should — `crates/app` is the
only crate that depends on GPUI. See [`docs/architecture/overview.md`](docs/architecture/overview.md)
for the target shape and why.

Built on **GPUI** (a pinned git dependency on [zed-industries/zed](https://github.com/zed-industries/zed),
see [below](#gpui-version-pin)), **[`alacritty_terminal`](https://github.com/zed-industries/alacritty)**
+ **`portable-pty`** for terminal/PTY, **[`gix`](https://crates.io/crates/gix)** + the real `git`
CLI for version control, **[`lsp-types`](https://crates.io/crates/lsp-types)** with a hand-written
client for `rust-analyzer`/`typescript-language-server`/Vue Language Tools/`pyright`/`gopls`, and
**`tree-sitter`** (Rust, TypeScript/TSX, Python) for syntax highlighting.

## System dependencies

**Linux** needs real dev packages for Wayland/X11/Vulkan/fontconfig:

```sh
sudo apt-get install -y \
  build-essential clang cmake pkg-config \
  libfontconfig-dev \
  libvulkan1 mesa-vulkan-drivers \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libasound2-dev
```

(Trimmed from upstream Zed's own [`script/linux`](https://github.com/zed-industries/zed/blob/main/script/linux)
— see `.github/workflows/ci.yml`'s Linux job comments for what was dropped and why.)

**macOS** needs Xcode Command Line Tools (`xcode-select -p` should print a path) and, if `cargo
build` fails with a Metal shader compilation error, `xcodebuild -downloadComponent
MetalToolchain`. No Homebrew packages — GPUI's macOS backend links Xcode's own system frameworks.

**Windows** needs no extra system packages beyond the Rust toolchain.

`/setup` (this repo's `.claude/commands/setup.md`, if you're using Claude Code) runs these checks
for you.

### GPUI version pin

`gpui`/`gpui_platform` are pinned via `rev =` in the root [`Cargo.toml`](Cargo.toml) to
`7b030b500810b04cf5fb4aa5973be99a502d9f36` — the exact commit this workspace was built and verified
against. A newer commit may or may not still build against this workspace's code; bump it
deliberately, not casually.

## Contributing

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

must all pass — see [`CLAUDE.md`](CLAUDE.md) for the full standards, [`CONTRIBUTING.md`](CONTRIBUTING.md)
for the process, and [`docs/development-workflow.md`](docs/development-workflow.md) for how a
change moves from issue to merged PR. `.github/workflows/ci.yml` runs the same checks on every
push. If you're using [Claude Code](https://claude.com/claude-code), this repo's `.claude/`
directory (skills, agents, commands) is set up already — install [rtk](https://github.com/rtk-ai/rtk)
and run `/setup` to cut token usage on command output; optional, not a build dependency.

## Contributors

- [Colin Espinas](https://github.com/ColinEspinas)
- [Lucas Boinet](https://github.com/lucasboinet)
- [Lucas](https://github.com/LucasPcq)

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option, matching the license of GPUI and this project's other key dependencies.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you shall be dual licensed as above, without any additional terms or conditions.
