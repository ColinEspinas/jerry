# Assessment

> **Addendum (inline git blame, GitHub issue #29):** the rest of this document is a
> point-in-time snapshot from an earlier, much smaller phase of this project (see its own
> line counts below) and is left as-is rather than rewritten. What follows is an honest,
> narrowly-scoped note on the one subsystem added in this session, not a refresh of the
> whole picture.
>
> **Real, end to end:** `wt_core::blame::blame_file`/`commit_message` run real `git blame
> --line-porcelain`/`git log` against real repositories (9 tests, real tempdir repos, no
> mocking) and correctly distinguish "not a repo"/"untracked file" from a genuine failure.
> The app-side current-line inline blame is wired to that real data, not a placeholder: it
> runs off the GPUI foreground thread, is cached per file and revision, recomputes via a
> throttled freshness poll (verified to cover a save; verified in code, not by hand-testing
> a real `git commit`/checkout from inside a running app window, that the same poll also
> catches a commit or branch switch - a real but slightly weaker chain of evidence than the
> save path, which has an explicit, direct trigger and its own docs say so). The hover
> tooltip's full commit message is real and lazily fetched, not truncated/faked.
> **Not built:** the issue's secondary gutter/full-file blame view - no settings field or UI
> element pretends to offer it. **Not independently visually verified:** unlike this
> document's own "watched it run, not just read code that claims it runs" standard for
> earlier steps, this session had no interactive GPUI window available to click into and
> screenshot - `cargo test`/`cargo clippy` on the two crates this change touches
> (`wt-core`, `app`) are the actual evidence; a full `cargo test --workspace` run in this
> session's sandbox hit a large number of pre-existing, unrelated failures (real `/proc`
> reads, real PTY behavior, real language-server process spawns, a Windows path-separator
> assertion in recently-rebased code) that reproduce identically with this change's files
> stashed out, so they're excluded from this feature's own verification rather than
> papered over.

Written at the end of an autonomous, multi-agent build session: ~10h40m of wall-clock
time (first commit 00:43, last commit 11:24, same day), roughly 2.4M+ tokens of subagent
work across builder/checker/finder delegations, 10 commits, 5546 lines of hand-written
Rust across three crates (`wt-core` 2395, `app` 2213, `pty-core` 938), 63 tests. This is
not a summary written to make the work look good. It is written to help decide whether
this approach is worth a year of investment, so it says what actually happened, including
the parts that don't reflect well on the process.

## What genuinely runs end to end

- **`wt-core`**: real. Enumerates git worktrees via `gix` against real repositories,
  creates/removes them via real `git` CLI invocations, and the dirty-worktree-removal
  refusal is real and tested against actual dirty working trees (tracked modifications and
  untracked files both block removal without `force`). 36 tests, all against real git
  repos in tempdirs, none mocked.
- **`pty-core`**: real. Spawns real OS processes on real PTYs, streams real output through
  a bounded channel, resizes, and kills the entire process tree (not just the direct
  child) on teardown, verified against a process that backgrounds a grandchild via
  `setsid`. 10 tests, including a real `/proc`-based orphan check.
- **The GPUI app**: real, and I watched it run, not just read code that claims it runs.
  Screenshots across three separate build steps show: a left sidebar listing this
  repository's actual worktrees; a center pane with a real spawned shell whose output
  renders through genuine `alacritty_terminal::Term` grid emulation (confirmed by watching
  a real interactive `claude` CLI session render its box-drawing welcome screen correctly —
  cursor-addressed redraw, not scrolling garbage); multiple simultaneous tabs that can be
  opened and closed, with tab-close actually killing the underlying process (verified via
  PID checks, not just UI state); a right sidebar that toggles between a real recursive
  file tree and a real diff view showing actual committed, staged, and untracked changes
  in one worktree against its detected base branch.
- **The diff view specifically** ended up more correct than it looked at first: three
  rounds of adversarial testing found and fixed a bug that misattributed diff content to
  the wrong filename, a bug that silently hid every untracked file, and a bug that broke
  under a common (not obscure) git config setting. All three are fixed and covered by
  regression tests built from the actual reproduction, not from guessing at a fix.
- **The code editor's minimap** (GitHub issue #30, added well after this document's original
  session - see `BUILD-LOG.md`'s "Minimap: a real, canvas-rendered overview" entry for the full
  design record) is real, not a decorative panel: it renders the open file's actual syntax-colored
  content at reduced scale from the same highlighted data the editor itself paints from, its
  viewport slider is genuinely draggable and click-to-jump genuinely scrolls the real editor, and
  its git-diff overlay reads the same real on-disk diff the gutter stripe does. Two disclosed,
  narrower limits, not fabricated ones: search-match overlays aren't implemented (this app has no
  find-in-file feature to source real matches from at all), and the "doesn't cost frames while
  scrolling" requirement was met structurally (a large-file gate, no re-highlighting) rather than
  proven with an actual `gpui::FrameTiming` measurement.

## What does not run, or is genuinely rough

- **The shipped default build was never visually screenshotted.** `crates/app/Cargo.toml`
  ships with only the `wayland` GPUI backend enabled, because the `x11` feature link-fails
  on this dev machine (missing `libxkbcommon-x11`/`libxcb-xkb`, no passwordless sudo to
  install them) — a real gap, not a preference. Every screenshot in this project's history
  was taken from a temporarily patched x11 build, reverted afterward each time. The
  wayland-only default's process log shows it initializing and opening a window without
  crashing, which is real evidence, but weaker evidence than a screenshot. **On a stock
  X11-only Linux desktop with no Wayland compositor, this binary will not open a window at
  all** unless the user adds system X11 dev libraries and enables the `x11` feature
  themselves. This is the single biggest gap between "works in this sandbox" and "works
  for a random user."
- **No merge action.** The product description says the user "reads their diffs, and
  merges the good ones" — the build steps only ever specified a *read-only* diff view, and
  that's all that was built. There is no in-app way to merge, apply, stage, or commit
  anything from the diff pane. This is consistent with what was actually asked for in the
  five build steps, but it's a real gap against the one-paragraph product pitch, and worth
  being explicit about: this app currently lets you *watch* agents and *read* their diffs;
  merging still happens in a separate terminal.
- **Terminal scrollback is not user-accessible.** `alacritty_terminal::Term` keeps real
  scrollback history internally, but nothing in the UI exposes it — you only ever see the
  live viewport. For a long-running agent session that's produced pages of output, there
  is no way to scroll up and read what already happened.
- **No settings, no agent CLI discovery.** "Claude" and "Codex" sessions are two hardcoded
  buttons that run `claude`/`codex` by bare name off `PATH`. There is no way to configure
  a different agent CLI, pass it flags, or point it at a binary not on `PATH`.
- **No packaging.** This runs via `cargo run` from a source checkout. There is no installer,
  no release build pipeline, no `.deb`/AppImage/anything. That was never in scope for the
  five steps, but it means "a desktop application" currently means "a Rust project a
  developer can build," not something you hand to a non-developer.
- **Independent verification of interactivity was incomplete in one case.** During the
  step 4 audit, the checker agent could not get synthetic X11 input (clicks, keystrokes)
  to reach the app window in this sandbox at all — a sandbox limitation, not an app bug —
  and so could not itself reproduce the builder's claimed interactive screenshots (typing
  into a live `claude` session, closing tabs by click). Those specific claims rest on the
  builder's own screenshots plus code review and deterministic unit tests, not on a second
  independent party actually clicking the buttons. Everything downstream (process
  teardown, grid rendering correctness) was still independently proven at the unit-test
  level, which is real evidence, but the chain of trust for "a human could actually use
  this with a mouse and keyboard" has one link that's weaker than the rest.
- Assorted smaller rough edges left in the code, all found by adversarial audits and
  either fixed or explicitly documented rather than hidden: a resize path with no upper
  bound on terminal grid size for a very large window; a base-branch comparison that uses
  branch names rather than checking whether the local default branch is itself behind its
  remote tracking branch.

## Which step was the real wall, and why

**Step 3 — the first GPUI window.** Not step 5 (diffs), despite step 5 having the most
individually severe *logic* bugs found. Step 3 is where every kind of risk this project
carried showed up at once: an unfamiliar, fast-moving UI framework with no stable
published API to lean on (only a single vendored snapshot); a headless-ish sandbox with no
Vulkan loader, requiring a fallback to Mesa's GL backend that happened to work; no
screenshot tooling installed, requiring `pip install --user mss` and later a hand-rolled
XWD-to-PNG parser when even that came back solid black against Wayland; and — this is the
part that matters most — a bug that was invisible by every automated signal available.
The first version of the terminal pane built cleanly, passed its own tests, and produced a
screenshot that looked correct, because a shell prompt has no trailing newline and
therefore happened to survive a CR/LF handling bug that silently deleted every other line
of real output. Nothing about "it compiles, tests pass, here's a screenshot" caught that.
Only an adversarial pass that fed a known string through the real pipeline and checked the
output character-for-character caught it. That step alone consumed roughly 5 million
agent-milliseconds of wall-clock time across its build/audit/fix cycle — more than any
other single step — and set the pattern the rest of the project followed: build, then
actively try to prove it's lying to you, because it usually is a little.

## Code-writing vs. figuring out the API

Rough, honestly: **something like 40% of total effort was spent figuring out what the
real API was**, not writing or fixing logic. This wasn't optional overhead — it was a hard
project rule ("never invent an API") that turned out to be load-bearing every single time
it was tested. Concrete examples: the entire GPUI entity/rendering model had to be derived
from reading `vendor/zed`'s own example files and panel crates, because GPUI has no stable
published documentation and has changed shape significantly across its history; the `gix`
crate is used nowhere in `vendor/zed` at all, so every worktree/merge-base call had to be
verified by reading the actual fetched crate source under `~/.cargo/registry/src/`; the
`alacritty_terminal` fork Zed depends on had to be read directly from its git checkout
because it's not the crate published on crates.io upstream; even `portable-pty`, a
comparatively simple and stable crate, had to be verified from source because Zed doesn't
use it (Zed drives `alacritty_terminal`'s own PTY layer directly). The remaining ~60% was
implementation, and of that, a meaningful fraction was re-work driven by adversarial
audits catching real bugs the first pass missed — not wasted effort, but effort that
wouldn't have been needed if the first draft had been correct.

## What I would sequence differently

Fix the terminal rendering foundation *before* step 3 was called done, not as an
emergency correction midway through step 4. The fixed stack named `alacritty_terminal`
explicitly; step 3 quietly substituted a much weaker hand-rolled text scanner to manage
GPUI's complexity, documented the substitution honestly, and it still took an outside
human catching it in conversation to trigger the real fix, one step later than it should
have. An audit step that explicitly checks "does the delivered thing match every named
technology in the fixed stack, not just 'does it look plausible'" would have caught this
without needing a human in the loop. Relatedly: I would have insisted the checker agent
actually attempt live interaction (real synthetic input, not just screenshots and unit
tests) *before* declaring a UI step done, and treated "the sandbox won't let me click
things" as a blocking finding to work around, not a note to route around silently.

I would also have written `.claude/agents/*.md` files for the builder/checker/finder
definitions from the very first message, rather than after losing them twice to session
reconnects mid-build. That cost real time re-deriving what had already been decided and,
once, required the human to paste the original launch configuration back to me — a fully
avoidable dependency on the user for something that was entirely my own infrastructure to
persist correctly from the start.

## Honest estimate: a solo developer, new to Rust, three months, with this kind of assistance

Not this far. A developer new to Rust would not reach a working three-pane GPUI app with
real terminal grid emulation, real git worktree management, and a correct diff viewer in
three months, even with constant AI assistance at this level of capability. The reasons
are specific, not general pessimism:

First, the API-verification discipline that made this project's actual output trustworthy
— reading vendored source, cross-checking against fetched crate internals, empirically
testing claims instead of trusting that code which compiles is code that works — is not
something a Rust beginner can do themselves, and it's exactly the thing an AI assistant
without that discipline imposed on it will skip, producing code that compiles, has tests,
and is still quietly wrong (this project's own first drafts prove that: every step but one
shipped at least one real bug past its own tests and code review, caught only by a second,
adversarial pass). A beginner working with an assistant that isn't forced through that
same adversarial cycle will accumulate exactly this kind of bug and have no way to know
it, because "it compiles and the demo looks right" is a false signal in a domain this
unforgiving — GPUI in particular has no stable documentation to fall back on, so both
human and assistant are reduced to reading vendored source line by line, which is slow and
unglamorous work that a beginner has no independent way to sanity-check.

Second, a meaningful fraction of this project's real time went to environment friction
that has nothing to do with Rust skill and everything to do with systems-level Linux
desktop knowledge: missing Vulkan loaders, Wayland-vs-X11 backend selection, WSLg's
specific rootless-Xwayland screenshot limitations, workspace-vs-workspace Cargo dependency
resolution edge cases. A beginner would hit every one of these and, without the ability to
independently diagnose "is this a code bug or an environment problem," would likely
misattribute much of it to their own code and thrash.

Third — and this is the part worth sitting with — the actual bottleneck across this
session was never "can an agent write the code." It was verification: proving a claim was
true rather than merely plausible. That loop (build, then adversarially try to disprove
it) is the entire reason this project's output is real rather than merely convincing. A
solo beginner directing an assistant without independently insisting on that loop, or
without the judgment to recognize when a "looks done" report should be distrusted, will
get further than three months of unassisted Rust learning would produce alone, but will
end up with something that runs in the demo and fails in ways neither they nor a casual
reading of the code would predict. Three months is enough time to get a beginner to a
working three-zone layout showing sample data and a shell that echoes text back — which is
exactly the kind of result this project's own rules ("no fake functionality," "mark it
stubbed instead") were written to rule out as a stopping point, not to reach as a finish
line.

## Addendum: multi-cursor editing (GitHub issue #28)

This file otherwise reflects only the original five-step build session and was never kept
current through the many revisions BUILD-LOG.md records afterward (R1–R12, the feature-folder
restructure, the file tree work) — checked directly: `git log -- ASSESSMENT.md` shows exactly
one commit, its own creation. This one paragraph is added because the multi-cursor work is a
real new subsystem, not because the rest of the file has been re-audited against everything
that shipped since; treat the sections above as a snapshot of the original build, not of the
project as it stands today.

**What's real**: the core data model (a primary cursor plus a `Vec` of secondary cursors on
`EditBuffer`), `Ctrl+D`'s two-step select-word/add-next-occurrence flow, `Ctrl+Shift+L`,
`Ctrl+K Ctrl+D`, Alt+click, Esc, simultaneous typing/pasting/backspacing/deleting across every
active cursor as one atomic edit, collision merging, and multi-cursor arrow-key movement are all
genuinely wired end to end — driven through the same real key-binding table and
`EntityInputHandler` path every other editing feature in this app uses, not a parallel or
special-cased mechanism, and painted with real per-cursor selection fills/caret bars, not a data
model with nothing visible backing it. This was rebased mid-implementation onto a separately-landed
branch (GitHub issue #17) that added this editor's first real text undo/redo, and the two are
genuinely integrated, not just made to compile side by side: a multi-cursor edit now really is one
`Ctrl+Z` step (`EditBuffer::apply_at_every_cursor` records every cursor's own splice into the same
history group), with one honest, documented exception — undo/redo restores all of the affected text
correctly, but only the *primary* cursor's own caret/selection precisely, since the shared
`SelectionSnapshot` type (used by every text-input widget in this app, not just the File view) has
no way to represent more than one cursor. 25 new tests, including four that drive the real bound
keystrokes rather than calling `EditBuffer` methods directly, and five covering the undo/redo
integration specifically (one-step coalescing, redo, the documented single-cursor-after-undo
outcome, and a multi-keystroke burst still coalescing to one group).

**What's genuinely not there**: Alt+Shift+drag column selection is absent because ordinary
mouse-drag-to-select doesn't exist in this editor at all yet, for any selection, single- or
multi-cursor. Multi-cursor support does not extend to the separate merge hand-edit surface
(`crate::merge::editing`) — a deliberate scope narrowing to the File view only, documented in
BUILD-LOG.md's own entry for this work. Undo/redo's own single-selection limitation above is a
real, live gap for the specific case of undoing a multi-cursor edit, not a hypothetical one.

**Independent verification note**: this work was built from a Windows sandbox instead of this
project's usual Linux/WSL2 environment. `lsp-core`'s own test target does not compile on Windows
at all (a pre-existing, unrelated `#[cfg(unix)]` gap, confirmed unaffected by this change by
reproducing it against a clean stash of the base commit too), which blocks a literal
`cargo test --workspace`/`cargo clippy --workspace --all-targets` run on this machine outright;
verified instead with `--workspace --exclude lsp-core` (clean) plus `-p app` on its own. Three of
this crate's own real-language-server-spawning test modules
(`lsp_hover_wiring_tests`/`lsp_diagnostics_wiring_tests`/`vue_two_server_wiring_tests`) also
couldn't run - `rust-analyzer` is not installed for the pinned toolchain here - and one real,
timing-sensitive fake-LSP-server test flaked under this sandbox's own resource constraints; none of
the four are anywhere near this change's own files. Every test this change actually added was run
and passed, and the rest of the `app` crate's suite passed in full alongside it - see BUILD-LOG.md's
own entry for the exact numbers and commands.
