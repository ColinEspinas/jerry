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
  session's detected base branch, and a real code editor (not read-only): full text
  editing with real cursor/selection/IME support, backed by a real LSP client (hover,
  go-to-definition, diagnostics, completions) generalized across several languages, not
  just Rust. Merge conflicts can be resolved either structurally (per-hunk accept/reject)
  or by hand-editing the raw conflict markers directly, using the same real editor.

There's also a command palette (with a real "History" group for undoing/redoing worktree-
level actions like committing or discarding a session's changes) and a settings surface
(including real Keybindings and Language Servers pages). See [BUILD-LOG.md](BUILD-LOG.md)
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
- **[GPUI](https://www.gpui.rs/)** for the UI, pulled in as a plain git dependency on
  [zed-industries/zed](https://github.com/zed-industries/zed), pinned to a specific
  commit (see [below](#gpui-version-pin)) — no local checkout or extra setup required,
  Cargo fetches it like any other dependency.
- **[`alacritty_terminal`](https://github.com/zed-industries/alacritty)** (the same fork
  and pinned revision Zed's own `terminal` crate uses) for real ANSI/grid terminal
  emulation, and **`portable-pty`** for spawning and managing the underlying PTY
  processes.
- **[`gix`](https://crates.io/crates/gix)** for read-only git operations (worktree/diff
  enumeration) and the real **`git` CLI** (via explicit argument vectors, never shell
  strings) for anything that mutates the repository (worktree add/remove, merges).
- **[`lsp-types`](https://crates.io/crates/lsp-types)** plus a hand-written client in
  `lsp-core` (not upstream Zed's own LSP client — see `crates/lsp-core/Cargo.toml` for
  why) that speaks to real, separately-installed language servers: `rust-analyzer`,
  `typescript-language-server` (TS/TSX/JS/JSX), Vue Language Tools, `pyright`, and
  `gopls`, auto-detected per-server from each server's own real `initialize` response
  (e.g. push- vs. pull-based diagnostics) rather than assuming one protocol style fits
  all of them. The Settings → Language Servers page has a real "Install" action linking
  to each server's own install docs.
- **`tree-sitter`**, with real grammars for Rust, TypeScript/TSX, and Python (the
  languages with dedicated `tree-sitter-*` crates wired up so far — see
  `crates/app/src/code_view.rs`), for syntax highlighting in the editor.

## Workspace layout

```
crates/
  wt-core/    git worktree management (gix reads, git-CLI mutations)
  pty-core/   real PTY process spawning, I/O streaming, resize, process-tree teardown
  lsp-core/   real LSP client (spawns and speaks LSP to rust-analyzer and other servers)
  app/        the GPUI application itself (binary crate)
assets/       bundled fonts (IBM Plex Sans/Mono, OFL) used by the app
design_handoff_jerry_ade/
              the design handoff this UI ("Jerry") was built from: a high-fidelity
              HTML mockup + design tokens, kept for reference during UI work — not
              something to port markup from, and not itself part of the app.
```

## Custom themes

Besides the six built-in themes (Jerry Dark, Jerry Dim, Slate, Ember, Moss, Paper), the Themes
settings page (Settings → Themes) supports user-authored ones too (GitHub issue #5) — built-in
and custom themes are shown as the same kind of card, not a first-class set and a second-tier
list. In fact, the six built-ins are themselves just files: `assets/themes/*.toml` in this
repository, embedded into the binary and parsed through the exact same code path a custom
theme's own file goes through, not a separate hardcoded palette.

**File format.** One `.toml` file per theme, five hex colours plus a name/subtitle:

```toml
name = "Midnight Coral"
subtitle = "warm accent, dark base"
background   = "#0c0d10"
panel        = "#181a1e"
accent_green = "#5cb87f"
accent_amber = "#e2a336"
accent_blue  = "#e07a5f"
```

Colours are `#rrggbb` — a `#` plus exactly six hex digits; no `#rgb` shorthand, alpha channel, or
named CSS colours. `name` must be unique (it can't reuse a built-in theme's own name); `subtitle`
is optional. Those five swatches are the same five every built-in theme is defined by — the rest
of the app's roughly 200 colour tokens are derived from how they differ from Jerry Dark's own
five (see `derive_shift` in `crates/app/src/theme.rs` for the actual HSL transform — it's a
private function, so this means reading the source, not generated rustdoc), so authoring five
colours re-skins the whole app, not just a preview card. `panel` also has to actually read as a
different shade from `background`: a real perceptual-brightness check rejects a `panel` that's
the same colour as (or only a couple of hex digits off from) `background`.

**Where files live.** `~/.config/jerry/themes/*.toml` — a `themes` directory sitting next to
`~/.config/jerry/settings.toml`. Every `.toml` file directly inside it is loaded as a theme at
startup; a file that fails to parse or validate is skipped with a real, specific error shown on
the Themes page (the rest of the directory still loads normally).

**Getting started without leaving the app.** The Themes page's "Custom themes" section has three
real actions: **New from template…** writes a real, well-commented starting-point file straight
into that directory (the same file checked into this repository at
[`assets/themes/template.toml`](assets/themes/template.toml) — copying it by hand works exactly
as well as clicking the button); **Import theme…** validates and copies in any `.toml` file you
already have, via a native file picker; **Export current theme…** saves whichever theme is
currently active to a file you can hand to someone else. Every custom theme card also has a
two-click **Remove** action that deletes its backing file.

## Building and running it

### GPUI version pin

`gpui` and `gpui_platform` are plain git dependencies on
[zed-industries/zed](https://github.com/zed-industries/zed), pinned via `rev =` in the
root [`Cargo.toml`](Cargo.toml) and in `crates/app/Cargo.toml` to
`7b030b500810b04cf5fb4aa5973be99a502d9f36` — the exact commit this workspace was built
and verified against. Cargo fetches and builds it like any other git dependency; no local
checkout or manual setup step is required.

A different (e.g. newer) `zed-industries/zed` commit may or may not still build against
this workspace's code — GPUI's API isn't stable across arbitrary upstream revisions. If
you deliberately bump it, update the `rev =` in **both** `Cargo.toml` files together and
expect to fix real compile errors, not just update a hash.

### System dependencies (Linux)

GPUI's Linux backend here builds both `wayland` and `x11` (see the comment on the
`gpui_platform` dependency in `crates/app/Cargo.toml` — Revision R12, `BUILD-LOG.md`'s
entry of the same name). The two are not mutually exclusive at compile time: with both
compiled in, `gpui::guess_compositor()` picks a backend for real at *runtime*, checking
`$WAYLAND_DISPLAY` first and falling back to `$DISPLAY` (bare X11) if that's unset, exactly
matching how upstream Zed itself ships. Building it needs real system dev packages —
Wayland client headers, X11/xkbcommon headers, Vulkan, fontconfig. On Debian/Ubuntu:

```sh
sudo apt-get install -y \
  build-essential clang cmake pkg-config \
  libfontconfig-dev \
  libvulkan1 mesa-vulkan-drivers \
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev
```

This list is derived from (a trimmed-down subset of) upstream Zed's own
[`script/linux`](https://github.com/zed-industries/zed/blob/main/script/linux)
Ubuntu/Debian dependency list — see that script for the authoritative, exhaustive version,
and the comment above the equivalent CI step in `.github/workflows/ci.yml` for what was
trimmed and why. `libasound2-dev`/`libssl-dev`/`libsqlite3-dev`/`libzstd-dev` were dropped
from this list after checking `Cargo.lock`: no `alsa`/`alsa-sys`, `openssl-sys`,
`libsqlite3-sys`/`rusqlite`, or `zstd-sys`/`libz-sys` appears anywhere in this workspace's
resolved dependency tree, so none of them are actually needed to build it.

`libxkbcommon-x11-dev` and `libx11-xcb-dev` were already on this list before `x11` was
actually enabled (Revision R1) — they were added preemptively and never trimmed back out.
That turned out to be exactly right: `apt-cache depends` shows `libxkbcommon-x11-dev` hard-
`Depends:` on `libxcb-xkb-dev`, and `libx11-xcb-dev` hard-`Depends:` on `libx11-dev` and
`libxcb1-dev` — so installing the two packages already listed above pulls in every other
X11/xkb `-dev` package the `x11` feature actually links against, with nothing further to
add. Verified for real (not just read off `apt-cache depends`) by building this crate
against those exact packages' real contents — see `BUILD-LOG.md`'s Revision R12 entry for
how, given this project's own sandbox still has no passwordless `sudo` to install them
system-wide, and what was and wasn't visually confirmed as a result.

Running the app needs a real display server it can reach — either a Wayland compositor
(`WAYLAND_DISPLAY` set) or an X11 display (`DISPLAY` set); this project's own development
happened under WSLg on Windows, which provides both. A stock Linux desktop running only one
of the two now works either way, which is the entire point of this revision — see
`BUILD-LOG.md`'s Revision R12 entry and `ASSESSMENT.md` for exactly what was and wasn't
confirmed visually under each backend in this project's own (WSLg) sandbox.

macOS and Windows are only build-tested in CI (see below) — nobody has run this app on
those platforms yet.

### Build and test

```sh
cargo build --workspace
cargo test --workspace
cargo run -p app
```

The commands above build the **debug** profile — correct for running the test suite and for
iterating on the app's own code (fast incremental compiles), but not representative of the
app's real performance: debug builds carry no optimizations and pay for debug assertions on
every frame's layout/paint/highlight work, on top of a real ~14x larger binary (roughly 680MB
vs. 50MB here). For actually using the app day to day, or for judging whether it feels
responsive, run the **release** profile instead:

```sh
cargo run --release -p app
```

This isn't optional polish - a debug-profile GPUI app is commonly 5-20x slower for the exact
CPU-bound work this app does every frame (GPUI's own layout/paint, `tree-sitter` parsing,
terminal-grid decode), and none of this project's own performance investigations or
measurements (see BUILD-LOG.md's several perf-focused revisions) are representative of what a
debug build feels like - they were all measured against release builds.

`cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check`
must also pass — see [CONTRIBUTING.md](CONTRIBUTING.md) for the full set of checks and
the project's other hard rules.

## Continuous integration

`.github/workflows/ci.yml` runs `cargo build`, `cargo test`, `cargo clippy -D warnings`,
and `cargo fmt --check` on Linux (the platform this project can actually verify end to
end), plus a build-only job on macOS and Windows. See that file's comments for the
reasoning behind the trimmed system-dependency list.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option, matching the license of GPUI and this
project's other key dependencies (see [BUILD-LOG.md](BUILD-LOG.md) for the stack's
licensing rationale).

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this project by you shall be dual licensed as above, without any additional
terms or conditions.
