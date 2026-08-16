<div align="center">

# Jerry

**Supervise a fleet of AI coding agents — each one in its own real git worktree.**

[![Release](https://img.shields.io/github/v/release/ColinEspinas/jerry?label=release&color=5cb87f)](https://github.com/ColinEspinas/jerry/releases/latest)
[![CI](https://github.com/ColinEspinas/jerry/actions/workflows/ci.yml/badge.svg)](https://github.com/ColinEspinas/jerry/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-74ade8)](#license)

[**Download**](../../releases/latest) &nbsp;·&nbsp; [Install from source](#install) &nbsp;·&nbsp; [Supported agents](#supported-agents) &nbsp;·&nbsp; [Contributing](CONTRIBUTING.md)

<img src="docs/images/hero.png" alt="Jerry: the session rail, an agent running in the work surface, and its diff on the right" width="880">

</div>

> [!NOTE]
> **Jerry is a working prototype, not a released product.** It does real work — real worktrees, real
> agent processes, real `git` — but expect rough edges. [Open issues](../../issues) is the honest
> list of what's in progress.

## The idea

You already give each task its own agent. Jerry gives each agent its own **real git worktree** — a
separate checkout on its own branch, not a sandbox and not a copy — plus a terminal to run in, a diff
of what it changed, and an editor to fix what it got wrong.

The rail on the left is every session at once. The rest of the window is whichever one you're looking
at.

## Features

### One worktree per session

<img src="docs/images/rail.png" alt="The session rail, one row per worktree and agent" width="720">

Every session is a real `git worktree` on its own branch, so two agents editing the same file never
collide. The rail shows all of them at once, with status derived from the underlying git and process
state rather than from anything the agent says about itself. Claude Code sessions report structural
status over a hook side-channel; other agents fall back to terminal-title and quiescence signals.

### A real terminal, not a log pane

<img src="docs/images/terminal.png" alt="An agent CLI running in the work surface" width="720">

The work surface runs the agent's own CLI in a real PTY, rendered through `alacritty_terminal` grid
emulation — cursor-addressed redraw, so the agent's live TUI behaves exactly as it does in your own
terminal instead of scrolling past as a text dump. Tabs are scoped per worktree, and a plain shell
tab sits alongside the agents in the same directory.

### Diff, with notes that go back to the agent

<img src="docs/images/review.png" alt="The diff view with line-anchored review notes" width="720">

Review a session's changes against its automatically detected base branch. Line-anchored notes batch
into a single prompt and get delivered to a *named* agent's PTY, then stay pinned afterwards so you
can check the revision against what you actually asked for.

### An editor with real language support

<img src="docs/images/editor.png" alt="The code editor with LSP diagnostics and completions" width="720">

Full text editing — cursor, selection, IME — with `tree-sitter` highlighting and a hand-written LSP
client behind it: hover, go-to-definition, diagnostics and completions from `rust-analyzer`,
`typescript-language-server`, Vue Language Tools, `pyright` and `gopls`. Agents get things wrong;
fixing them shouldn't mean leaving the window.

### Merge conflicts, structurally

<img src="docs/images/conflicts.png" alt="Structural per-hunk merge conflict resolution" width="720">

Resolve a conflicted merge per hunk with accept/reject, or drop into the same editor and edit the
conflict markers by hand. Both paths are the real resolution — whichever one suits the conflict in
front of you.

### The git graph

<img src="docs/images/graph.png" alt="The git graph tab" width="720">

Commits, branches and worktrees in one view, with an interactive rebase mode whose plan you edit
before anything runs.

### Also in the box

- **Undo/redo for worktree-level actions** — committing or discarding a session's changes is
  reversible from the command palette's History group.
- **Per-agent line provenance** — when two agents share a worktree, which one wrote each line.
- **Agent run history** — a repo → worktree → run index with full transcripts, and `--resume` to pick
  a previous Claude Code conversation back up.
- **Rate-limit budget readout** — per-provider 5h/7d usage, next to the agent pane.
- **Search** across the worktree, in the right panel.
- **Themes** — six bundled, plus VS Code theme import. Themes are plain TOML where every key is
  optional and anything unset is inherited, so a three-line file is a complete theme. See
  [`docs/themes.md`](docs/themes.md).
- **Sounds** for agent events — off by default, end to end.
- **Self-update** from GitHub releases.
- **Settings** for General, Agents, Worktrees, Appearance, Themes, Keybindings, Editor, Language
  servers, Notifications and Integrations.

## Supported agents

| Agent | CLI | Status signal |
| --- | --- | --- |
| **Claude Code** | `claude` | Structural, over a hook side-channel. `--resume` reattaches a previous conversation. |
| **Codex** | `codex` | Inferred from terminal title and quiescence. |
| *Plain shell* | your default shell | Not an agent — an ordinary terminal tab in the same worktree. |

Both CLIs are resolved on `$PATH` and spawned directly in the session's worktree. Jerry doesn't
bundle, wrap or proxy either one, so you bring your own install and your own auth; **Settings ›
Agents** tells you whether each is actually on your `$PATH`.

[Cursor CLI support](https://github.com/ColinEspinas/jerry/issues/353) is open, not built.

## Install

**Prebuilt binary** — from the [latest release](../../releases/latest):

| Platform | Asset |
| --- | --- |
| macOS (Apple Silicon) | `jerry-macos.tar.gz` |
| Linux (x86_64) | `jerry-linux.tar.gz` |
| Windows (x86_64) | `jerry-windows.zip` |

All three ship a binary every release, but maturity is uneven: macOS has been run locally on Apple
Silicon, Linux builds and runs against both display servers, and Windows is build-verified in CI.

**From source:**

```sh
git clone https://github.com/ColinEspinas/jerry
cd jerry
cargo run --release -p app [path-to-a-repo]
```

Uses the toolchain pinned in [`rust-toolchain.toml`](rust-toolchain.toml). Build with `--release`
unless you're actively recompiling — [`CLAUDE.md`](CLAUDE.md#commands) covers why a debug GPUI build
isn't worth measuring. Linux needs real system dev packages before that build will succeed; macOS
needs Xcode Command Line Tools, and Windows needs nothing extra:

<details>
<summary><strong>System dependencies</strong></summary>

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
— see `.github/workflows/ci.yml`'s Linux job comments for what was dropped and why.) GPUI's Linux
backend compiles both the `wayland` and `x11` features and picks between them at runtime from
`$WAYLAND_DISPLAY`/`$DISPLAY`.

**macOS** needs Xcode Command Line Tools (`xcode-select -p` should print a path) and, if `cargo
build` fails with a Metal shader compilation error, `xcodebuild -downloadComponent MetalToolchain`.
No Homebrew packages — GPUI's macOS backend links Xcode's own system frameworks.

**Windows** needs no extra system packages beyond the Rust toolchain.

`gpui`/`gpui_platform` are git dependencies pinned by `rev =` to one exact
[`zed-industries/zed`](https://github.com/zed-industries/zed) commit; the revision itself lives in
the root [`Cargo.toml`](Cargo.toml), which is its only home. Cargo fetches it automatically.

If you're using [Claude Code](https://claude.com/claude-code), `/setup` runs all of these checks for
you.

</details>

## Built with

[GPUI](https://www.gpui.rs/) (Zed's UI framework) ·
[`alacritty_terminal`](https://github.com/zed-industries/alacritty) + `portable-pty` for the
terminal and PTY layer · [`gix`](https://crates.io/crates/gix) plus the real `git` CLI for version
control · [`lsp-types`](https://crates.io/crates/lsp-types) with a hand-written client ·
`tree-sitter` (Rust, TypeScript/TSX, JavaScript, Python) for highlighting.

## Contributing

Issues and PRs are welcome. [`CONTRIBUTING.md`](CONTRIBUTING.md) covers the process and what has to
pass before a PR opens; [`CLAUDE.md`](CLAUDE.md) is the single source of truth for how the code
itself should look, for humans and agents alike.

The workspace is four crates — `wt-core` (git), `pty-core` (processes), `lsp-core` (language
servers) and `app` (the GPUI application, and the only crate allowed to depend on GPUI). The rules
behind that split, and the reasoning for each, are in
[`docs/architecture/`](docs/architecture/overview.md); how a change moves from issue to merged PR is
in [`docs/development-workflow.md`](docs/development-workflow.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option, matching the license of GPUI and this project's other key dependencies.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you shall be dual licensed as above, without any additional terms or conditions.
