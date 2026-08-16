<h1 align="center">Jerry</h1>

<p align="center">
  <a href="https://github.com/ColinEspinas/jerry/releases/latest"><img src="https://img.shields.io/github/v/release/ColinEspinas/jerry?style=flat&label=release&color=5cb87f" alt="Latest release" /></a>
  <a href="https://github.com/ColinEspinas/jerry/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/ColinEspinas/jerry/ci.yml?style=flat&branch=master&label=CI" alt="CI status" /></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-74ade8?style=flat" alt="License: MIT OR Apache-2.0" /></a>
  <img src="https://img.shields.io/badge/macOS%20%7C%20Linux%20%7C%20Windows-4493F8?style=flat" alt="Supported platforms: macOS, Linux, Windows" />
  <a href="https://www.gpui.rs/"><img src="https://img.shields.io/badge/built%20with-GPUI-e2a336?style=flat" alt="Built with GPUI" /></a>
</p>

<p align="center">
  <strong>Supervise a fleet of AI coding agents.</strong><br/>
  Run Claude Code and Codex side by side — each in its own real git worktree, all in one window.
</p>

<h3 align="center"><a href="../../releases/latest"><ins>Download Jerry</ins></a></h3>

<p align="center">
  <img src="docs/images/hero.png" alt="Jerry: the session rail, an agent running in the work surface, and its diff on the right" width="960" />
</p>

<p align="center">
  <sub><a href="#install">Install</a> &nbsp;·&nbsp; <a href="#supported-agents">Supported agents</a> &nbsp;·&nbsp; <a href="docs/architecture/overview.md">Architecture</a> &nbsp;·&nbsp; <a href="CONTRIBUTING.md">Contributing</a> &nbsp;·&nbsp; <a href="../../issues">Issues</a></sub>
</p>

> [!NOTE]
> **Jerry is a working prototype, not a released product.** It does real work — real worktrees, real
> agent processes, real `git` — but expect rough edges. [Open issues](../../issues) is the honest
> list of what's in progress.

## One window, every agent

You already give each task its own agent. Jerry gives each agent its own **real git worktree** — a
separate checkout on its own branch, not a sandbox and not a copy — then puts a terminal, a diff, an
editor and a git graph around it.

- **Every agent gets a worktree.** Two agents editing the same file never collide.
- **Every session in one rail.** Status comes from the underlying git and process state, not from
  what the agent says about itself.
- **Review without leaving.** Diff against the detected base branch, drop line-anchored notes, send
  them back to the agent that wrote the code.
- **Fix it yourself when that's faster.** A real editor with LSP, in the same window.
- **Resolve the merge in place.** Per-hunk accept/reject, or the conflict markers by hand.

---

## Features

<table>
<tr>
<td width="50%" valign="middle">

### One worktree per session

Every session is a real `git worktree` on its own branch. The rail shows all of them at once, with
status derived from the underlying git and process state — Claude Code reports structurally over a
hook side-channel, other agents fall back to terminal-title and quiescence signals.

[Source →](crates/app/src/rail/)

</td>
<td width="50%">
  <img src="docs/images/rail.png" alt="The session rail, one row per worktree and agent" width="100%" />
</td>
</tr>
<tr>
<td width="50%" valign="middle">

### A real terminal, not a log pane

The agent's own CLI runs in a real PTY, rendered through `alacritty_terminal` grid emulation —
cursor-addressed redraw, so its live TUI behaves exactly as it does in your own terminal. Tabs are
scoped per worktree, with a plain shell tab alongside the agents.

[Source →](crates/app/src/work_surface/)

</td>
<td width="50%">
  <img src="docs/images/terminal.png" alt="An agent CLI running in the work surface" width="100%" />
</td>
</tr>
<tr>
<td width="50%" valign="middle">

### Diff, with notes that go back

Review a session's changes against its automatically detected base branch. Line-anchored notes batch
into a single prompt, get delivered to a *named* agent's PTY, and stay pinned afterwards so you can
check the revision against what you actually asked for.

[Source →](crates/app/src/review_notes/)

</td>
<td width="50%">
  <img src="docs/images/review.png" alt="The diff view with line-anchored review notes" width="100%" />
</td>
</tr>
<tr>
<td width="50%" valign="middle">

### An editor with real language support

Full text editing — cursor, selection, IME — with `tree-sitter` highlighting and a hand-written LSP
client behind it: hover, go-to-definition, diagnostics and completions from `rust-analyzer`,
`typescript-language-server`, Vue Language Tools, `pyright` and `gopls`.

[Source →](crates/app/src/code_surface/)

</td>
<td width="50%">
  <img src="docs/images/editor.png" alt="The code editor with LSP diagnostics and completions" width="100%" />
</td>
</tr>
<tr>
<td width="50%" valign="middle">

### Merge conflicts, structurally

Resolve a conflicted merge per hunk with accept/reject, or drop into the same editor and edit the
conflict markers by hand. Both paths are the real resolution — whichever suits the conflict in front
of you.

[Source →](crates/app/src/merge/)

</td>
<td width="50%">
  <img src="docs/images/conflicts.png" alt="Structural per-hunk merge conflict resolution" width="100%" />
</td>
</tr>
<tr>
<td width="50%" valign="middle">

### The git graph

Commits, branches and worktrees in one view, with an interactive rebase mode whose plan you edit
before anything runs.

[Source →](crates/app/src/graph_view/)

</td>
<td width="50%">
  <img src="docs/images/graph.png" alt="The git graph tab" width="100%" />
</td>
</tr>
</table>

**Also in the box:**

- **Undo/redo for worktree-level actions** — committing or discarding a session's changes is
  reversible from the command palette's History group.
- **Per-agent line provenance** — when two agents share a worktree, which one wrote each line.
- **[Agent run history](crates/app/src/run_history/)** — a repo → worktree → run index with full
  transcripts, and `--resume` to pick a previous Claude Code conversation back up.
- **[Rate-limit budget readout](crates/app/src/budget/)** — per-provider 5h/7d usage, next to the
  agent pane.
- **[Search](crates/app/src/search/)** across the worktree, in the right panel.
- **[Themes](docs/themes.md)** — six bundled, plus VS Code theme import. Plain TOML where every key
  is optional and anything unset is inherited, so a three-line file is a complete theme.
- **[Sounds](crates/app/src/sound/)** for agent events — off by default, end to end.
- **[Self-update](crates/app/src/updater/)** from GitHub releases.
- **Settings** for General, Agents, Worktrees, Appearance, Themes, Keybindings, Editor, Language
  servers, Notifications and Integrations.

---

## Supported agents

<p>
  <a href="https://claude.com/claude-code"><kbd><img src="https://www.google.com/s2/favicons?domain=claude.com&sz=64" alt="" width="16" valign="middle" /> Claude Code</kbd></a> &nbsp;
  <a href="https://github.com/openai/codex"><kbd><img src="https://www.google.com/s2/favicons?domain=openai.com&sz=64" alt="" width="16" valign="middle" /> Codex</kbd></a> &nbsp;
  <kbd>Plain shell</kbd>
</p>

| Agent | CLI | Status signal |
| --- | --- | --- |
| **Claude Code** | `claude` | Structural, over a hook side-channel. `--resume` reattaches a previous conversation. |
| **Codex** | `codex` | Inferred from terminal title and quiescence. |
| *Plain shell* | your default shell | Not an agent — an ordinary terminal tab in the same worktree. |

Both CLIs are resolved on `$PATH` and spawned directly in the session's worktree. Jerry doesn't
bundle, wrap or proxy either one, so you bring your own install and your own auth; **Settings ›
Agents** tells you whether each is actually on your `$PATH`.

Unlike the tools Jerry is modelled on, this is a **short, explicit list, not "any CLI agent"** — each
one is a real enum variant with its own spawn and status handling.
[Cursor CLI support](https://github.com/ColinEspinas/jerry/issues/353) is open, not built.

---

## Install

### Prebuilt binary

From the [latest release](../../releases/latest):

| Platform | Asset |
| --- | --- |
| macOS (Apple Silicon) | [`jerry-macos.tar.gz`](../../releases/latest) |
| Linux (x86_64) | [`jerry-linux.tar.gz`](../../releases/latest) |
| Windows (x86_64) | [`jerry-windows.zip`](../../releases/latest) |

All three ship a binary every release, but maturity is uneven: macOS has been run locally on Apple
Silicon, Linux builds and runs against both display servers, and Windows is build-verified in CI.

### From source

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

---

## Built with

[GPUI](https://www.gpui.rs/) (Zed's UI framework) &nbsp;·&nbsp;
[`alacritty_terminal`](https://github.com/zed-industries/alacritty) + `portable-pty` for terminal and
PTY &nbsp;·&nbsp; [`gix`](https://crates.io/crates/gix) plus the real `git` CLI for version control
&nbsp;·&nbsp; [`lsp-types`](https://crates.io/crates/lsp-types) with a hand-written client
&nbsp;·&nbsp; `tree-sitter` (Rust, TypeScript/TSX, JavaScript, Python) for highlighting.

## Contributing

Issues and PRs are welcome. [`CONTRIBUTING.md`](CONTRIBUTING.md) covers the process and what has to
pass before a PR opens; [`CLAUDE.md`](CLAUDE.md) is the single source of truth for how the code
itself should look, for humans and agents alike.

The workspace is four crates — `wt-core` (git), `pty-core` (processes), `lsp-core` (language servers)
and `app` (the GPUI application, and the only crate allowed to depend on GPUI). The rules behind that
split, and the reasoning for each, are in [`docs/architecture/`](docs/architecture/overview.md); how
a change moves from issue to merged PR is in
[`docs/development-workflow.md`](docs/development-workflow.md).

<a href="https://github.com/ColinEspinas/jerry/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=ColinEspinas/jerry" alt="Jerry contributors" />
</a>

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option, matching the license of GPUI and this project's other key dependencies.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
this project by you shall be dual licensed as above, without any additional terms or conditions.
