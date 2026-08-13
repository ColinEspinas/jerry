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

## Themes

Jerry's entire interface is painted from about 270 named colour tokens (`crates/app/src/theme.rs`),
grouped into modules — `surface`, `border`, `text`, `status`, `syntax`, `term`, `diff`, `editor`,
`graph`, and so on. A **theme** is a file that names any subset of those tokens; everything it
doesn't name is inherited from the theme it declares as its `base`, and ultimately from Jerry
Dark's own compiled-in defaults.

There are six built-in themes (Jerry Dark, Jerry Dim, Slate, Ember, Moss, Paper) and any number of
user-authored ones (GitHub issue #5). Both kinds are shown as the same kind of card on Settings →
Themes, and both are literally the same kind of file: the built-ins live at `assets/themes/*.toml`
in this repository, embedded into the binary and parsed through the exact same code a custom
theme's own file goes through. Jerry Dark's own file names no colours at all — it *is* the
compiled default palette; the other five are complete, literal, hand-editable palettes.

**File format.** One `.toml` file per theme. Jerry writes them with section headings and a
comment on most keys — the comments are pulled from the colour tokens' own doc comments in
`crates/app/src/theme.rs`, so they can't drift from what the code says — and reads them liberally:
key order, grouping and comments carry no meaning, so a hand-edited file never has to look like a
generated one.

```toml
name = "Midnight Coral"
subtitle = "warm accent, dark base"
base = "Jerry Dark"

# The five swatches this theme's card shows on the Themes page.
preview = ["#0c0d10", "#101216", "#5cb87f", "#e2a336", "#e07a5f"]

# ──────────────────────────────────────────────────────────────────────────────
# Surfaces and structure
# ──────────────────────────────────────────────────────────────────────────────

# Backgrounds - every solid fill in the app, from the window itself down to
# popovers, hover states and keycaps.
[surface]
window       = "#0c0d10"  # window body
rail         = "#101216"  # left rail + right panel
card         = "#181a1e"  # composer, settings cards
row_selected = "#1b1f26"

# ──────────────────────────────────────────────────────────────────────────────
# The code surface
# ──────────────────────────────────────────────────────────────────────────────

[syntax]
keyword  = "#ff79c6"
string   = "#f1fa8c"
variable = "#bd89a5"
```

- **`name`** — required, non-empty, and must not reuse a built-in theme's name.
- **`subtitle`** — optional one-line description shown on the card.
- **`base`** — optional; the theme every unnamed key is inherited from. `"Jerry Dark"` is the usual
  choice, and omitting it is equivalent. A `base` chain that loops is rejected with a real error
  naming the whole chain.
- **`preview`** — optional array of five `#rrggbb` colours for the card's swatch strip. Omitted, it
  is read from the theme's own `surface.window`/`surface.rail`/`status.review`/`status.ask`/
  `status.run`.
- **every other table** is a `crate::theme` module, and every key in it is one of that module's
  tokens with its Rust constant name lowercased: `theme::surface::WINDOW` is `[surface] window`,
  `theme::syntax::FUNCTION_METHOD` is `[syntax] function_method`. Pair and array tokens use a
  quoted dotted key inside their table (`"sonnet.fg"`, `"lanes.0"`).

Colours are `#rrggbb` — a `#` plus exactly six hex digits; no `#rgb` shorthand, alpha channel, or
named CSS colours. An unknown table or key is a real, specific rejection naming what it didn't
recognise, never a silently ignored typo.

**A theme naming three keys is a complete, valid theme**, and stays valid as Jerry grows: keys
added by future versions simply inherit, so a file never has to be kept exhaustive. Deleting a line
is a real, supported edit — that key goes back to what it inherited.

The one thing Jerry insists on is that text is legible: if body text or code would be effectively
invisible against the surface behind it (below 1.6:1 contrast), the theme is rejected with an error
saying which pair failed. Nothing else about a palette is second-guessed — flat designs that
separate regions with borders rather than brightness (VSCode's own Dark Modern, for one) are
perfectly fine.

For the full list of real keys, open any bundled theme —
[`assets/themes/slate.toml`](assets/themes/slate.toml) and its siblings are complete, commented
palettes, and copying one is the fastest way to author a whole theme.
[`assets/themes/template.toml`](assets/themes/template.toml) is a smaller commented starting point.

**Where files live.** `~/.config/jerry/themes/*.toml` — a `themes` directory sitting next to
`~/.config/jerry/settings.toml`. Every `.toml` file directly inside it is loaded as a theme at
startup; a file that fails to parse, validate, or resolve its `base` is skipped with a real,
specific error shown on the Themes page (the rest of the directory still loads normally).

**Getting started without leaving the app.** The Themes page's "Custom themes" section has five
real actions: **New from template…** writes the commented starting-point file above straight into
that directory; **Import theme…** validates and copies in any `.toml` file you already have, via a
native file picker; **Import VSCode theme…** converts a downloaded VSCode theme `.json` file (see
below) the same way; **Generate from colour…** takes one hex colour and derives a whole theme from
it (see below); **Export current theme…** saves whichever theme is currently active to a file you
can hand to someone else. Every custom theme card also has a two-click **Remove** action that
deletes its backing file.

**Generate from colour.** Type a `#rrggbb` seed into the Themes page's own input and click
Generate: Jerry rotates its whole palette so its accent blue lands on that hue, scales saturation
to match, leaves lightness alone (so the theme's light/dark structure survives), and writes the
result out as a real, complete, literal theme file — all ~270 keys, ready to hand-tune line by
line. This is the same HSL derivation (`derive_shift`/`apply_shift` in `crates/app/src/theme.rs`)
that used to compute every non-Jerry-Dark colour live on every render; it is now strictly an
authoring tool that produces files, never part of live rendering.

**Importing a VSCode theme (GitHub issue #141).** "Import VSCode theme…" picks a real VSCode theme
JSON file (JSONC — `//`/`/* */` comments and trailing commas are tolerated, since that's how most
real downloaded theme files are actually written) and converts it into a real Jerry theme file, in
two layers:

- **A complete derived base.** Five representative colours (`editor.background`, a sidebar/panel
  background, and three accents from keys like `terminal.ansiGreen`/`terminal.ansiYellow`/
  `button.background`) are run through the same derivation "Generate from colour" uses, giving
  every one of Jerry's ~270 tokens a real value in the theme's own family. This is what stops an
  imported light theme from leaving half the chrome dark.
VSCode's own default themes are defined as deltas on each other (`Dark+` is `tokenColors` plus
`"include": "./dark_vs.json"`, and `Dark Modern` includes *that*), so the importer follows an
`include` chain relative to the file's own directory, with the including file winning on `colors`
and its `tokenColors` appended after the base's. Every shipped VSCode default — Dark+/Light+,
Dark/Light Modern, the `_vs` bases — plus Monokai, Solarized Dark and One Dark Pro is imported
end-to-end by this crate's own tests, against the real, unmodified upstream JSON.

- **Every directly-mapped key on top.** Jerry's tokens are mapped onto the VSCode `colors` keys
  that genuinely mean the same thing — the editor surface, gutter, selection and line highlight;
  sidebar/activity bar/panel/status bar/title bar; list hover and selection rows; input and widget
  surfaces; buttons and badges; the terminal ANSI palette; diff and git decoration colours; error
  and warning foregrounds; scrollbar slider states; and the `foreground`/`descriptionForeground`/
  `disabledForeground` text levels. Syntax comes from the theme's own `tokenColors`: every
  highlight bucket searches for its real textmate scope (`entity.name.function` for `function`,
  `keyword.control` for `keyword`, and so on), with proper scope matching — a rule for
  `variable.parameter` colours parameters without also recolouring plain variables — and a bucket
  with no rule of its own inherits its parent bucket's resolved colour.

VSCode colour families with no counterpart in this app (peek view, notebooks, testing,
merge-conflict decorations, debug toolbar, charts, bracket-pair colourisation) are deliberately not
mapped; those tokens keep their derived value, which is still a real colour in the imported theme's
own family. The result is an ordinary Jerry theme file — every value literal, every line editable
afterwards.

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
  libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libasound2-dev
```

This list is derived from (a trimmed-down subset of) upstream Zed's own
[`script/linux`](https://github.com/zed-industries/zed/blob/main/script/linux)
Ubuntu/Debian dependency list — see that script for the authoritative, exhaustive version,
and the comment above the equivalent CI step in `.github/workflows/ci.yml` for what was
trimmed and why. `libssl-dev`/`libsqlite3-dev`/`libzstd-dev` were dropped from this list
after checking `Cargo.lock`: no `openssl-sys`, `libsqlite3-sys`/`rusqlite`, or
`zstd-sys`/`libz-sys` appears anywhere in this workspace's resolved dependency tree, so
none of those three are actually needed to build it. `libasound2-dev` was dropped for the
same reason at the time, but GitHub issue #226's sound design module (`crate::sound`) now
pulls in `rodio`'s `playback` feature, which depends on `cpal`, which links ALSA on Linux
(`alsa-sys` now genuinely appears in `Cargo.lock`) — so it's back on this list rather than
a silent Linux build failure the moment that dependency lands.

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

Windows is still only build-tested in CI (see below) — nobody has run this app there yet.
macOS has now been run for real, once, locally (Apple Silicon, Xcode 26.2).

### System dependencies (macOS)

No Homebrew packages needed — GPUI's macOS backend links Xcode's own system frameworks
(AppKit, Metal, CoreText, …), not separate `-dev` packages. Two things to have in place first:

**Xcode Command Line Tools**

```sh
xcode-select -p   # should print a path, not an error
```

**A Metal Toolchain.** GPUI compiles its shaders at build time, and a stock Xcode install
doesn't always have the toolchain that needs. If `cargo build` fails with:

```
metal shader compilation failed: ... cannot execute tool 'metal' due to missing Metal Toolchain
```

fetch it directly:

```sh
xcodebuild -downloadComponent MetalToolchain
```

If that itself fails with a plug-in/framework-loading error (a stale Xcode install, unrelated
to this project), repair Xcode first, then retry the download:

```sh
xcodebuild -runFirstLaunch
```

**Text rendering.** `gpui_macos` ships real system-font matching behind its own `font-kit`
feature, off by default. Without it, the app builds and runs with no error but paints a
blank window — no text, no crash. This repo's `crates/app/Cargo.toml` already requests it
for macOS, so a plain `cargo build`/`cargo run` renders text out of the box.

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
