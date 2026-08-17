# Zone 1 — the agent rail

- **Code:** `crates/app/src/rail/`
- **Tokens:** `theme::{status, rail, band, zone}`

## What it's for

The rail is the product. Everything else in the window is a place you go *after* the rail has told
you where to go.

Its single job is answering **"who needs me"** in under a second, across every repository you have
open and every worktree in each of them. The answer is delivered pre-verbally: the most urgent
things float to the top and wear a colour, so the eye resolves *how much amber is at the top* before
it reads a single word. See [`principles.md`](./principles.md) rule 1.

## Structure

### Two levels, always

**Repo group → worktree → agents.** There is no grouping-mode toggle. An earlier design had a `by
urgency / by project` switch in the rail header; the revision that redesigned the rail around repo
grouping removed it outright, along with its sort control, rather than leaving a second structure
behind. `rail::state`'s own module docs record this.

Urgency did not disappear — it became **ranking within the fixed structure** rather than an
alternative to it:

- Worktrees inside a repo are ordered by `WorktreeRow::urgency_rank` — their most urgent agent.
- Repos are ordered by their own most urgent worktree.
- A worktree's aggregate status is its most urgent agent's status (`WorktreeRow::aggregate_status`).

That gets the same "amber at the top" property as the old urgency mode while keeping one structure a
user can build a spatial memory of.

### The parts, top to bottom

| Part | Height | What it is |
|---|---|---|
| Sidebar strip | `theme::band::CHROME_HEADER` | View switcher — see below |
| Filter row | `theme::band::FILTER_ROW` | Filters the rows below; follows the active view |
| Body | fill | The active view |
| Footer | — | Worktree inventory and the prune action |

**Repo header** — name, `N wt`, and **two** urgency counts, not one. Red for worktrees holding a
failed agent, amber for worktrees holding an asking one, each hidden at zero. Merging them into a
single amber count "said *three worktrees want you* when one of the three had actually died, and
amber is the wrong colour for that" — see the general rule in
[`layout.md`](./layout.md#rules-that-matter).

**Worktree row** — always present whether or not it is expanded, carrying its branch, its aggregate
status, and its diff totals against base.

**Agent row** — one per open agent under an expanded worktree, in the same order the tab strip uses.
Carries the agent's tint badge, its title, its status, and its own `+n −n`.

**Earlier-runs link** — `↺ N earlier runs`, under a worktree that has **no live agent**.
Deliberately under the row rather than among its children, so a folded worktree still offers it; and
deliberately not under every worktree, because a first pass that did produced eight identical rows
carrying no information. Clicking it goes to the History view.

`rail::state::RailListItem` is the flattened list of all of the above — index-only into the frame's
own `&[RepoGroup]`, rebuilt each render rather than cached.

### The sidebar strip

A **horizontal** icon strip along the top of Zone 1 (`rail::strip`, `rail::strip_render`), not a
vertical activity bar: a vertical bar costs permanent width on a window whose entire job is fitting
three panels side by side.

Two cells — **Worktrees** and **Problems** — plus `+` and `⋯`. `SidebarView` has a third variant,
**History**, which is a real view with a real body and is deliberately *absent* from
`SidebarView::ALL`: it lives in the `⋯` overflow instead, because a permanent cell in the strip is a
claim that you switch to it constantly. Search is not here at all; it is the middle tab of Zone 3.

Cells are `theme::zone::SIDEBAR_STRIP_CELL` wide, full-height, no radius and no gap — the same
object as the centre tabs, which is what makes the dividing rules read as a tab strip rather than as
icons scattered on a dark band.

### Status derivation

`rail::status::derive_status` turns already-read process signals into a `Status`. It is pure and
window-free, so every rule is directly testable.

The signal it refines with is unusual and worth knowing about: `rail::title_signal` reads the agent
CLI's **own terminal title**. TUI programs have written their state into the terminal title since
long before agent CLIs existed, and agent CLIs reuse the convention — so a terminal that reads the
title learns what the agent is doing the moment the agent decides it, instead of inferring it from
how long the pty has been quiet. That module's docs record what was verified live on which CLI
versions, including a captured nine-second stretch of real work with zero pty output — exactly the
false positive the title signal exists to prevent.

A second, structural channel exists for Claude Code specifically: its own hook system
(`crates/app/src/hooks/`), which fires at named lifecycle events and reports *"blocked on a human"*
as a fact rather than an inference.

## Rules that matter

- **Colour on a rail row means status, and nothing else.** The left edge, the dot, and the question
  preview's tint all come from `Status::color`. A decorative accent here costs the rail its
  glanceability. ([`principles.md`](./principles.md) rule 2.)
- **The strip empties itself on an empty day**, and it does so *at the source*
  (`strip::strip_view_cells` returns an empty `Vec`), not with a `when(..)` in the renderer. With no
  worktrees there are no views to offer, and a switcher with dead views is worse than no switcher.
  Gating in the renderer once meant the cells were hidden and their badges were not — claiming three
  agents needed a human on a day the rail, title bar and footer all reported zero.
- **Two states are never summed.** A repo header shows failed and asking as separate counts.
- **Header counts read `all_rows`, never the filtered `rows`.** A filter narrows what you see; it
  must not change what the header claims exists.
- **An errored worktree row is never interactive and never shows children.** It renders itself and
  stops.
- **`rows_loaded` must be consulted before treating an empty list as "zero worktrees".** Not-yet-
  loaded and genuinely-empty are different states and get different copy.
- **Worktree inventory and its prune action belong here**, not in the status bar — one fact, one
  home.

## Not built yet

- **Question previews depend on what the agent exposes.** The rail shows *that* an agent is asking,
  from the title signal, the hook channel, or quiescence. Reading the tail of the pty to preview the
  question itself is only as good as the CLI's own output; there is no cross-CLI structured channel
  for it.
- **Only Claude Code has the structural hook channel.** Every other CLI is on the title-signal plus
  quiescence heuristic, which is genuinely coarser. `hooks/` documents the shape a second
  integration would take.
