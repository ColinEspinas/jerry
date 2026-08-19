# Zone 2 — the work surface

- **Code:** `crates/app/src/work_surface/`, and the surfaces it hosts — `terminal/`,
  `code_surface/`, `lsp/`, `merge/`, `graph_view/`, `review/`, `review_notes/`, `run_history/`,
  `provenance/`, `budget/`
- **Tokens:** `theme::{term, editor, diff, syntax, band, button, completions_popup}`

## What it's for

Zone 2 is where you actually do the thing the rail sent you to do. It is a **tab strip over one
surface region** — never a split, never a floating pane. Which surface is showing is entirely a
function of which tab is active, and the chrome above the surface (the tab strip, the agent context
bar) is constant so that switching between an agent pane and a diff does not move anything you were
about to click.

## Structure

### The stack

Described in [`layout.md`](./layout.md#the-work-surface-stack): tab strip · agent context bar ·
optional conflict banner · surface · one footer.

### The tab strip

A **real tab list**, not a three-way pane selector. `work_surface::state::TabRef` is the identity of
one tab:

| `TabRef` | Payload | Cardinality |
|---|---|---|
| `Agent(AgentId)` | the agent | one per open agent (CLI or shell) |
| `File(PathBuf)` | the file | one per open file, in open order |
| `Graph` | — | at most one per window |
| `Review(AgentId)` | the agent reviewed | at most one open, but *per agent* — that's the point of the feature |
| `Run` | — | one run tab per worktree; opening another replaces it |

The payload rule is worth stating because it is not arbitrary: a variant carries a payload exactly
when two of it can meaningfully coexist in one strip. `Run` carries none, so that "replaced" keeps
the tab's dragged position instead of closing and reopening at the end of the strip.

Tab order is per worktree and persisted. `reconcile_tab_order` is pure and idempotent: it drops
entries that no longer exist and appends anything newly open at the end — which is what lets the
render path call it fresh every frame instead of maintaining a mutated cache. A real drag is the
only thing that writes back.

Each tab carries a chip (see [`vocabulary.md`](./vocabulary.md#chips)): the agent's tint for a CLI
tab, the terminal glyph for a shell, the file's language chip for a file tab — so a tab is always
colour-coded to *who* or *what* it is.

**Opening a file adds a tab.** It never takes over the centre region. Sources: a row in Changes, a
file in the tree, a `path:line` reference in terminal output, a palette result, `]` / `[`.

The `+` is a menu button opening a popover of *New terminal · New agent pane · Open file… · Next
changed file*, each with its own hint keycaps.

### The agent context bar

Agent badge · agent name · divider · branch · worktree path · status pill · actions.

**The layout rule that matters:** every child is inflexible and non-wrapping except the worktree
path, which is the single flexible item and ellipsises. Without that the bar wraps and overflows the
moment the centre zone narrows. ([`principles.md`](./principles.md) rule 6.)

### Surface A — the agent pane

**There is no chat UI.** The agent runs in a real pty (`pty-core` + `alacritty_terminal` via
`terminal::pane` and `terminal::grid`) and Jerry renders its output verbatim, cell by cell. The
agent's question is *its own* numbered prompt, never a card Jerry designed for it.

That is a product position, not an implementation shortcut, and it is why the pane's header shows
the real invocation, the real pid and the real pty state (`attached · waiting on stdin`, `exited
101`, `detached · resumable`) rather than an abstraction over them.

Its footer band is a **readout, not an action bar** — the strip below an agent pane reports; it does
not offer git operations the CLI could have done itself. `work_surface::state::ActionKind` is down
to one variant (`Respawn`) and the six it lost were deleted rather than hidden - `DiscardWorktree`
last, in GitHub issue #462, which left worktree removal with a single home on the rail's worktree
row.
Anything still listed renders honestly disabled when it has no backing logic
(`FooterAction::implemented`).

The pane also carries the **per-provider rate-limit budget** readout (`budget/`) — the `claude 5h
▓░░░░░ 19%` bar and the popover behind it, read from each provider's own usage endpoint with real
credentials found on disk.

### Surface B — the shell

Same geometry as the agent pane, with its own info footer (`theme::band::PTY_INFO_FOOTER`) carrying
pid, grid dimensions and the environment chip. A pane gets one footer or the other, never both.

Terminal output is **interactive**: `terminal::links` is a pure `path:line[:col]` scanner over
already-rendered row text, and a match renders as a link that opens the file as a tab. The link is a
span *inside* the line, not a whole-line style, so `↳ tests/upload.rs:88:` links only the path.

### Surface C — code (Diff | File)

The largest subsystem in the app; `code_surface/mod.rs`'s own docs are the file-by-file map. One
tab, two views, toggled in the surface's own toolbar.

**Diff view** — unified, full width. Two line-number gutters, a sign column, then the code.
Backgrounds and text per line kind come from `theme::diff`. **Folds** are what stop a twelve-file
change from being a wall: only changed hunks plus context, everything else collapsed behind a `⋯ N
unchanged lines` marker.

**File view** — a real editable buffer (`edit_buffer`, driven from real keystrokes through
`editing`'s `EntityInputHandler` wiring), with a breadcrumb, tree-sitter syntax spans from
`code_view`, a git gutter, bracket folding (`fold`), inline git blame (`blame`, `blame_view`), and a
syntax-coloured **minimap** with a draggable viewport slider.

Indentation resolves through `indent`, including a real minimal `.editorconfig` reader — Jerry does
not guess a file's indent style when the repository has stated it.

**The toolbar's `Accept file` is always rendered**, dimmed when there is nothing to accept. See
[`principles.md`](./principles.md) rule 6 for why this one is load-bearing.

**Zoom** is per editor tab (`zoom`), applied through a rem-scoped subtree (`root::rem_scope`) so the
diff and file views scale together while gutters and sign columns keep their fixed widths. Its
readout was removed from the status bar; the keyboard bindings were not.

### Language-server UI

`lsp/` owns the client and the pure response→view-model mappings; `code_surface::lsp_ui` draws hover
and diagnostics *over* the surface; `lsp::completion_popup` is the completions popup.

Three decorations, at most one at a time: **completions** (a two-column popover — candidates with
kind chips on the left, signature and doc on the right — carrying the one shadow in the product,
`theme::shadow::POPOVER`), **diagnostics** (an underline on the offending span, a dim inline
message, a tinted row, and a card below with the server's own code and any quick fix), and **hover**
(an underline on the symbol and a card with signature, doc and module path).

The mappings are pure functions over `lsp_types` responses, so their rules are tested against
fixture responses rather than against a live server.

### Surface D — merge conflict

Reached from the conflict banner. `merge/` holds a pure conflict/segment/choice model (`state`), the
real `wt_core::merge` calls and the surface's own state machine (`flow`), a whole-file hand-edit
buffer for conflicts the side-picker can't resolve (`editing`), and the view (`render`).

**Each side is headed by its agent, not by "ours" and "theirs".** That is the single most important
thing about this surface: in Jerry the two sides of a conflict are two agents you know by name and
by tint, and each column tints its own lines with that agent's colour. "Ours/theirs" is git's
framing and it is the wrong one here.

Jerry proposes the answer rather than only presenting the choice — a pre-flight strip states how
many files it can auto-resolve because their edits don't overlap, and `Take both` is the primary
action where both edits can be kept.

### Other centre surfaces

- **Graph** (`graph_view/`) — the git graph tab: lane canvas, row list, ref chips, and its own
  interactive-rebase mode. Lanes and merge elbows are hand-drawn Jerry vocabulary, not icons.
- **Review** (`review/`) — the agent review tab. The distinction it exists to draw is worth reading
  in that module's docs: a **git diff** answers *how does this worktree differ from the merge-base*
  (a property of the branch), while an **agent diff** answers *what did this agent change* (a
  property of a run). They are separated by base point and lifetime, not by filtering one list.
- **Review notes** (`review_notes/`) — line-anchored comments on a diff, **batched** into one
  prompt, delivered to a **named** agent's pty, and **kept pinned** afterwards so the revision can
  be checked against them. All three properties are the point; sending comments one at a time makes
  an agent swing back and forth.
- **Run transcript** (`run_history/`) — one finished run's own recording. The governing sentence is
  *"the sidebar indexes; the centre shows one run"*: history is a list you navigate from Zone 3, and
  opening an entry opens its transcript here, exactly the Explorer → editor pattern.
- **Provenance** (`provenance/`) — per-agent line provenance in a shared worktree: gutter tints,
  author chips, and the `⚠` shared-file ring. Deliberately not blame — blame answers *which commit*,
  and collapses every uncommitted line into one bucket; in a worktree with two agents running, the
  interesting fact about a dirty line is *which agent wrote it*.

## Rules that matter

- **One region, one surface.** Tabs replace each other. There is no split, and adding one is a
  [`decisions.md`](./decisions.md) entry.
- **A pane has exactly one bottom bar**, chosen by its kind. Never both stacked.
- **The agent pane renders the agent, not a rendering of the agent.** No chat bubbles, no
  synthesised cards around its questions, no reformatting of its output.
- **A tab's chip identifies it.** Agent tint for agents, language chip for files — derived, never
  hand-assigned.
- **Controls dim, they don't vanish.** `Accept file` is the canonical case.
- **Only the worktree path flexes in the context bar.** Everything else is `flex: none`.
- **Pure logic stays out of `render.rs`.** Every folder here already splits window-free logic from
  the `gpui::Div`-building code, and the tests live against the pure half. New work follows the
  split.

## Not built yet

- **No resumable agent state.** `ActionKind::Respawn` closes the tab and spawns a fresh agent of the
  same kind and cwd — an approximate stand-in for `Retry`/`Resume`, because there is no saved agent
  session to actually resume *from*. `pty_state_label` names the same gap.
- **The render layer still calls adapters directly.** Several `render.rs` files here reach into
  `wt_core::`/`pty_core::` rather than dispatching a Command/Query. This is a known architectural
  gap, ratcheted by `.claude/hooks/check-conventions.sh` and tracked in
  [`docs/architecture/decisions.md`](../architecture/decisions.md) §3 — not a pattern to copy.
