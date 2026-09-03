# Changelog

## v0.2.0 — Cursor agents, and a Windows that behaves

### New features

- **The Cursor agent CLI, as a first-class agent** — `cursor-agent` joins Claude and Codex: it spawns from `$PATH`, gets a real row in Settings › Agents with a live installed state, and genuinely resumes. Its `--resume` spelling is not Claude's, and it has no hook side-channel to learn a session id from, so a Cursor spawn mints its own chat id (`cursor-agent create-chat`) and starts attached to it. What isn't wired up stays honest: there's no Cursor rate-limit budget, because there's no endpoint to read one from.
- **A start-agent button that lets you pick the agent** — every door that started an agent (the `Start an agent` CTA, the `+` menu's `New agent` row, the title bar, `mod+shift+N`) resolved the kind for you first-installed-wins, so on a machine with several CLIs installed the others were reachable only through the command palette. `Start an agent` is now a split button whose caret opens a picker listing every kind with its real installed state.
- **Cursor agent status through hooks** — a Jerry-spawned `cursor-agent` reports status through the same hook side-channel Claude Code already uses, instead of being guessed at from terminal titles and quiescence. Jerry's forwarder entries are merged into your own `~/.cursor/hooks.json` — a real read-modify-write, never a clobber — behind a default-off `agents.cursor_hooks_enabled` toggle on the Agents settings page.
- **An agent's tab closes when its process exits cleanly** — typing `exit`, `/quit` or Ctrl-D left a dead pane in the tab strip that only a manual close would clear. A crash, a kill, or any non-zero exit still keeps its pane, so the last output and the footer's Retry/Resume are there to read.
- **Hovering a cut-off tab reveals its full title** — whether the label really was truncated is decided by real font metrics, the same predicate GPUI uses to draw the ellipsis, so a label that fits gets no tooltip at all.

### Improvements

- **Windows release builds no longer storm the screen with console windows.** Going GUI-subsystem wasn't paired with `CREATE_NO_WINDOW` on child spawns, so every git, LSP and `cmd` child allocated its own visible conhost — continuously, from launch. Every non-PTY child now goes through one constructor that suppresses it, enforced by a clippy lint rather than by convention.
- Spawned agents no longer outlive a force-killed Jerry on Windows. Every cleanup path was code running *inside* Jerry, which a crash or a Task Manager kill never reaches, so live agents survived as orphans — around 180 a day on the reporting machine. The process now joins a kill-on-close job object at startup, so the kernel takes every agent PTY, `new_std_command` child and language server down with it. The updater's relaunch is the one deliberate exception.
- Closing an agent pane on Windows no longer hangs: the writer held the last handle keeping `ClosePseudoConsole` from running, so the reader stayed blocked in `read` forever. A 120-second hang is now a 4.2-second clean teardown.
- A Windows POSIX shell (Git for Windows, MSYS2, Cygwin) starts as a login shell, so `/etc/profile` runs and `ls`, `cat`, `grep`, `clear` and `rm` exist — previously every one of them was `command not found` while builtins like `cd` kept working.
- Repository paths are canonicalized without Windows' verbatim `\\?\C:\…` spelling, which git rejects outright — that was failing every shadow-index diff on every poll, and a `repos.toml` written by an affected build heals itself on first save.
- The Files tree and command-palette candidates now come from git's own content answer instead of an uncapped, gitignore-blind filesystem walk that re-walked `target/` and `vendor/` (~300k entries) at least every 5 seconds. Deliberate visible change: gitignored files and empty directories no longer appear.
- The worktree watcher stops re-dirtying itself — the app's own diff tempfiles, written inside the git dir it watches, were holding `git worktree list` at its ~500 ms fast cadence forever.
- The Changes reload's poll re-runs its ~10 git spawns only when a zero-spawn fingerprint says something actually changed, and the highest-frequency spawn of all (`git rev-parse --git-path index`, twice inside every diff) is gone.
- Discarding a worktree kills everything Jerry started inside it — agent PTY sessions and the worktree's language servers — *before* deleting the directory, and refuses to spawn into a worktree whose delete is in flight.
- Worktree removal has one entry point, and it refuses a repository's main checkout up front. All three doors offered it there and failed after the fact — after already killing every live agent in the checkout.
- Revisiting a worktree paints its last-known file tree and Changes the instant it's selected, instead of blanking to empty-and-Loading for the second it took the directory walk and five git queries to land.
- The command palette scrolls to follow the keyboard selection, instead of letting it walk off the bottom edge where `enter` ran a row you couldn't see.
- The worktree root has a real drop target of its own. "Move this to the top level" was reachable only in a tree short enough to leave empty space below it — that is, never, in a real repository.
- The periodic update check no longer reaches GitHub during tests, so the suite doesn't depend on network reachability or GitHub's rate limiter.
- A debug `target/` is a good deal smaller: dependencies build with no debug info at all, and this workspace's own crates keep line tables — enough for file/line in panics and backtraces.
- `design_handoff_jerry_ade/` is replaced by an evolving `docs/design/` set: ten flat files, each mapping to one code module. Roughly a third of the old bundle's citations pointed at `revision N/` directories that were never committed.

**Full Changelog**: https://github.com/ColinEspinas/jerry/compare/v0.1.2...v0.2.0

## v0.1.2 — Real app bundles, and a terminal that hears the mouse

### New features

- **Real, launchable app bundles on all three platforms** — Jerry now ships as a real `Jerry.app` and DMG on macOS, a `.desktop` launcher entry with a full icon set on Linux, and a Windows executable carrying its own icon and version resource. Finder no longer has to host the binary in Terminal to launch it, Windows no longer opens a console window alongside it, and the macOS menu bar says "Jerry" instead of "app".
- **The mouse works in the terminal** — clicks, hovers and drags now reach the program running in an agent shell (xterm mouse reporting). An interactive TUI like Claude Code responds to the mouse instead of ignoring it; when a program asks for no reporting, text selection behaves exactly as before.

### Improvements

- A GUI-launched Jerry resolves your real login-shell `PATH`, so agent detection, language-server detection and the sidebar's availability checks find the same binaries they would from a terminal — Homebrew, `~/.cargo/bin`, nvm and the rest.
- The Changes pane and its counter refresh from an agent's own writes, instead of only when you switch to that tab. The list no longer sits frozen at whatever the worktree looked like the last time you opened it, which read as "some files are missing" rather than as staleness.
- The test suite is green and back in both the pre-commit gate and CI, running under `cargo nextest` so a hung test fails on its own instead of stalling the whole run.
- The README is a real product page, with images and badges.

**Full Changelog**: https://github.com/ColinEspinas/jerry/compare/v0.1.1...v0.1.2

## v0.1.1 — Settings that persist on Windows

### Improvements

- Settings load and save on Windows. The settings path resolved `$HOME` only, which Windows does not set, so saving silently did nothing and every launch fell back to an in-memory default — edits appeared to apply but were never written. It now resolves `%USERPROFILE%` there. Runs launched from Git Bash or WSL2 were unaffected, which is why this went unnoticed.

**Full Changelog**: https://github.com/ColinEspinas/jerry/compare/v0.1.0...v0.1.1

## v0.1.0 — Search, review, and a real history view

### New features

- **Search tab** — a real right-panel Search surface: a match tree over the whole worktree, gitignore-aware and cancellable, in-place replace, and `mod+F` in-file find. The exclude pattern list is now real, user-editable settings rather than a fixed list.
- **Diff-line review notes** — leave a note on any diff line, draft it, batch several, and send them straight to the run's agent.
- **Interactive rebase, for real** — the rebase plan surface got its full design pass: real drag-to-reorder, real rebase-onto from the branch context menu, and merge actions gated with a reason shown on the row.
- **History overhaul** — a sidebar run index, per-run transcript tabs, and a real view of outcomes and drift between runs.
- **Per-agent attribution** — a diff gutter and author chips show which agent wrote which line, with a shared-file ring filter to isolate one agent's changes.
- **Provider rate-limit budget** — a real per-provider budget readout in the agent pane, showing percentage used instead of nothing.
- **Text inputs, unified** — every input surface (search, rename, commit message, review notes, terminal) now shares one row with real selection, clipboard, and mouse editing.
- **Status bar redesign** — three tiers plus a Resources popover, replacing the old flat bar.
- **Changes panel redesign** — four collapsible sections in one scroller, git status letters, and floating hover actions per row.
- Sound design for app start and agent status changes; a shared menu system powering the rail's context menus; the Phosphor icon set vendored in with a shared render helper; cross-platform per-process CPU/memory sampling on macOS and Windows.

### Improvements

- Git graph: topological walk so lines never disconnect, survives fractional display scale factors, right-click on a branch chip opens the branch menu (not the row menu).
- Terminal: real mouse-wheel/PageUp scrollback, correct back-tab (Shift+Tab) sequence, resize no longer leaves an idle pane's grid stale, empty panes stop scrolling.
- Rail: virtualized Worktrees row list (fixes slow hover with many rows), agent/history-run selection is now mutually exclusive, every row gets a real width constraint.
- One pluralisation helper (`plural::count`/`plural::form`) now backs every count shown in the window.
- Command palette can now be opened with no tab open; macOS reopens a window when the Dock icon is clicked with none open.
- A long tail of layout fixes: tab strip real-scroll and spacer cleanup, sidebar indent guides no longer pick up the accent colour, caret/blink-loop fixes, icon glyph stretching, agent-pane context bar and readout strip.

**Full Changelog**: https://github.com/ColinEspinas/jerry/compare/v0.0.3...v0.1.0
