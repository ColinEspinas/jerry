# Design documentation

What Jerry's UI is *for*, surface by surface — the intent, the structure, the vocabulary and the
invariants a change must not break. Its counterpart is
[`docs/architecture/`](../architecture/overview.md), which covers where code lives; this set covers
what the code draws and why.

## The one rule that keeps this current

**These documents never reprint a value.** No hex codes, no pixel literals, no font sizes. Every
value in Jerry's UI already lives in `crates/app/src/theme.rs` as a named
[`ColorToken`](../../crates/app/src/theme.rs) or a `Pixels` constant, with its own doc comment, and
`theme.rs` is what the themes in `assets/themes/` and [`docs/themes.md`](../themes.md) are generated
against. These docs name the token; the token carries the number.

That is the whole reason the set can stay honest. A document that duplicates `#e2a336` goes stale
the first time somebody retunes the amber. A document that says *"the needs-input amber
(`theme::status::ASK`)"* cannot.

The same applies to structure: a surface doc names the module that draws it and describes the rules
that module has to hold, rather than re-deriving its layout.

## How to read it

Start with [`principles.md`](./principles.md) — six rules that constrain every other page — then
[`vocabulary.md`](./vocabulary.md) for the shared visual language (status, agent tints, chips,
keycaps, icons). After that, read the surface you're changing.

| File | Covers | Code it maps to |
|---|---|---|
| [`principles.md`](./principles.md) | The durable visual rules that constrain every UI change | — |
| [`vocabulary.md`](./vocabulary.md) | Status vocabulary, agent tints, chips, pills, keycaps, icons | `theme.rs`, `rail/status.rs`, `keymap.rs`, `icons.rs` |
| [`layout.md`](./layout.md) | Window bands, the three zones, the work-surface stack | `root/`, `title_bar/`, `status_bar/` |
| [`rail.md`](./rail.md) | Zone 1 — the agent rail, its strip, grouping and rows | `rail/` |
| [`work-surface.md`](./work-surface.md) | Zone 2 — tab strip, context bar, and every centre surface | `work_surface/`, `terminal/`, `code_surface/`, `lsp/`, `merge/`, `graph_view/`, `review/`, `run_history/` |
| [`sidebar.md`](./sidebar.md) | Zone 3 — Files · Search · Changes | `sidebar/`, `search/` |
| [`settings.md`](./settings.md) | The Settings surface, its nav, cards and row controls | `settings/` |
| [`command-palette.md`](./command-palette.md) | The ⌘P overlay, scopes and result rows | `palette/` |
| [`decisions.md`](./decisions.md) | Numbered design decisions log | — |

Every surface doc uses one skeleton, so the set reads as a single document:

```markdown
# Session rail

- **Code:** `crates/app/src/rail/`
- **Tokens:** `theme::{status, rail}`

## What it's for       — one paragraph, the job this surface does
## Structure           — the parts, and what each is
## Rules that matter   — the non-obvious invariants a change must not break
## Not built yet       — honest gaps, linked to their issues
```

`Not built yet` is the section that turns this from a description into a contribution map. It is the
same honesty [`CLAUDE.md`](../../CLAUDE.md)'s "no fake functionality" rule already demands of the
code, applied to the docs.

## How to change it

A UI change **updates the relevant page in the same PR** — that is the entire difference between
this and what it replaced. Concretely:

- Changed how a surface behaves or what it's composed of → edit that surface's `Structure` or `Rules
  that matter`.
- Built something the page lists under `Not built yet` → delete the line.
- Made a call that a future contributor would otherwise re-litigate, or reversed one already
  recorded → add a **new numbered entry** to [`decisions.md`](./decisions.md). Never edit an old
  entry back to "current"; mark it superseded and point at the new one.
- Retuned a colour or a dimension → that is a `theme.rs` change, and these docs should need no edit
  at all. If one is needed, the doc was reprinting a value it shouldn't have.

## What this replaced

Until #414, the design authority was `design_handoff_jerry_ade/` — a one-shot handoff bundle
containing a 4,266-line interactive HTML mockup, a transcribed `tokens.rs`, and a README written as
a build brief. It did its job: essentially all of Jerry's chrome was built from it. But it was
frozen by construction, so every design change after it was recorded as another `revision N/` folder
that was never committed, and roughly a third of the 364 source comments citing it pointed at paths
that did not exist in the repository.

The bundle is preserved by git history alone — no tag, no release asset. To recover it:

```sh
git log --diff-filter=D --format='%H' -1 -- design_handoff_jerry_ade/   # the deleting commit
git show <that-sha>^:design_handoff_jerry_ade/README.md                 # any file, as it was
```

Read it as the historical artefact it is. Where it and the shipped app disagree, **the app wins**,
and the delta is either an entry in [`decisions.md`](./decisions.md) or a line under a `Not built
yet` heading here.
