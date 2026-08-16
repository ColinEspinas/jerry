# Command palette

**Code:** `crates/app/src/palette/`
**Tokens:** `theme::palette`, `theme::{band::PALETTE_*, zone::PALETTE_WIDTH, shadow::PALETTE}`

## What it's for

One overlay that is both the command list **and** the way you get to a pane or a file without
touching the mouse. It is the keyboard route to everything, which is why an agent, a shell, a file
and a command are all rows in the same list rather than four separate finders.

An **overlay, not a page**: a flat scrim over everything below the title bar (a plain alpha fill, not
a blur — see [`principles.md`](./principles.md) rule 3), and a panel pinned near the top. Clicking
the scrim or pressing `esc` dismisses it. There is nothing here you can leave the app parked in.

## Structure

Bound to `mod+P`, deliberately not `mod+K`. `mod+K` was the original binding, but the file editor's
real `ctrl-k ctrl-d` chord registers `ctrl-k` as a chord *prefix* in that context, so a lone press
waited out GPUI's ~1s prefix timeout before reaching the palette — a real, noticeable delay. `mod+P`
also matches the VS Code / Sublime convention directly. The tradeoff is explicit: a focused
terminal's own readline `Ctrl+P` is shadowed by this binding.

### Scopes

`palette::state::PaletteScope` — `All` (default), `Commands`, `Files`. Reachable three ways, all
equivalent: clicking the segmented control, cycling with `⇥`, or typing the scope's own prefix
character (`›` or `@`) into an empty query (`typed_scope_prefix`).

### Steps are not a fourth scope

`PaletteStep` is the palette's one drill-down shape, entered by running a command that needs an
argument — today, "which language server?" for a restart. The distinction from a scope is
load-bearing and the code states it: **a scope is a user-switchable filter over the same candidates
and is reachable at any time; a step lists something else entirely and is left with `esc`, which
there means "back", not "close".**

### Groups

Built by `build_groups`, in order:

| Group | Scope | Notes |
|---|---|---|
| Agents | `All` | Real agent sessions only |
| Terminals | `All` | Shells, **split out rather than filtered out** |
| Commands | `All`, `Commands` | |
| Git | `All`, `Commands` | A dedicated group, not folded into Commands |
| Recent Files / Files | `All`, `Files` | Label depends on the query — see below |

The Agents/Terminals split is a good example of the honesty rule applied to a label. A shell is not
an agent: the rail gives it no agent row, and the pane chrome draws it a different bottom bar. Filing
it under a heading reading `Agents` would tell the user the opposite of what the rest of the app
tells them. Filtering shells out entirely would have been the other easy answer, and it was rejected
— the palette is the keyboard route to a pane, so that would make terminal tabs mouse-reachable only.
Splitting is the fix; renaming is the whole fix.

**"Recent Files" means "currently has uncommitted changes."** Jerry has no file-access or mtime
history to rank true recency by, so an empty query narrows to changed files under that label rather
than inventing an ordering. A non-empty query searches the whole tree under a plain `Files` label.

### Rows

A 15px kind chip — the `›` command chip, the file's language chip, or the agent's tint badge, so the
palette inherits the rail's colour coding — then the label with the **matched substring highlighted**
(three spans: pre, match, post), a dim secondary line (branch for an agent, `dir · +n −n` for a file,
a one-line description for a command), an optional status dot, and a keycap for the bound shortcut.

Selected rows carry the same left-edge treatment used elsewhere for selection.

## Rules that matter

- **Every `PaletteCommand` maps one-to-one to a real `AdeApp` method.** Never a stub, never a row
  that opens a "coming soon". `PaletteCommand::ALL` is the closed list.
- **A label must be true.** If a group's name would misdescribe half its rows, split the group; do
  not filter the rows away to make the name fit.
- **Scope ≠ step.** A new filter over the same candidates is a scope. A new list reached by running a
  command is a step. Getting this wrong makes `esc` mean two things.
- **The palette inherits existing chips**; it does not define its own row iconography.
- **No blur.** The scrim is a flat alpha fill.

## Not built yet

- **No fuzzy ranking across kinds.** Matching is a substring match per group; there is no unified
  relevance score that would let a file outrank a command.
- **No true recency.** See above — there is no access history to rank by, and the label is honest
  about the proxy it uses instead.
