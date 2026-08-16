# Design principles

Six rules. They constrain every other page in this set, and a UI change that breaks one needs an
entry in [`decisions.md`](./decisions.md) saying so — not a quiet exception.

## 1. The rail answers "who needs me" in under a second

Jerry exists to supervise several agents at once. Everything else in the window is secondary to the
left rail telling a developer, at a glance, which of six-plus running agents is blocked on them.

The mechanism is deliberately pre-verbal: every level of the rail is ranked by urgency
(`rail::state::WorktreeRow::urgency_rank`, `rail::status::Status::urgency_rank`) and every row
carries a coloured left edge in its status colour. The question the eye answers is *how tall is the
amber block at the top* — not *read the labels*. Any change that makes the answer require reading is
a regression, however much information it adds.

## 2. Colour is reserved for status and diffs

Nothing decorative is coloured. The status palette (`theme::status`), the diff palette
(`theme::diff`), the agent tints (`theme::agent`) and syntax highlighting (`theme::syntax`) are the
whole budget. Chrome is grey.

This is what makes rule 1 work: amber means one thing in this product. Spending amber on a
non-status accent costs the rail its glanceability everywhere at once. The agent tints are
constrained by the same logic — `theme::agent`'s own module docs record the reserved-hue rule and
the reallocation it forced (see [`decisions.md`](./decisions.md) §9).

## 3. Flat surfaces, one shadow

Flat fills, 1px borders, small radii. No blur, no layered transparency, no gradient. Borders carry
elevation, not shadows.

`theme::shadow` holds the exceptions — the completion popup, the command palette, and the shared
dropdown/menu shadow — and that module's own docs say to drop them if GPUI makes them awkward,
because the borders already do the work. The origin of this rule is the build target: the mockup was
authored inside GPUI's constraints so that nothing in it would need reinterpreting as a CSS effect
GPUI has no cheap equivalent for.

## 4. No fake functionality, on screen as much as in code

A control that looks clickable does something. A readout shows a real measurement. Sample data never
ships.

This is [`CLAUDE.md`](../../CLAUDE.md)'s rule applied to pixels, and it has real teeth in the
codebase: `work_surface::state::FooterAction::implemented` renders an unwired action *dimmed and
non-interactive* rather than hiding it or letting it no-op, and
`work_surface::state::ActionKind`'s own docs record five variants being **deleted** rather than left
as decoration. Where a surface genuinely isn't built, it says so — in the UI, and in the
`Not built yet` section of its page here.

## 5. One object, one specification

When two places draw the same thing, they read one constant. `theme::band::CHROME_HEADER` is the
canonical case: the work-surface tab strip, the rail's sidebar strip and the Zone 3 panel header all
sit on one y, and that constant's own doc comment records what happened when they didn't — "a
visible staircase at every column boundary". Its rule, kept verbatim in the code:

> Column headers that share a y are one rule, not three. Changing any of their heights changes a
> line that spans the whole window — check the other two in the same edit.

The corollary is that replacing a control means deleting its old keys in the same edit. A key
defined twice is two specifications of one thing, and the reader cannot tell which is real.

## 6. Layout must not move under the cursor

Controls do not appear and disappear with view state; they dim. The code surface's `Accept file`
button is the canonical case — it is always rendered, dimmed when there is nothing to accept,
because appearing and disappearing reflows the Diff/File toggle sitting next to it, under the
pointer that was about to click it.

The same reasoning drives the fixed widths in the agent context bar: every child is inflexible
except the worktree path, which ellipsises, so narrowing the centre zone never causes the bar to
wrap.

---

## Type, spacing and shape

Not principles so much as the palette these principles are executed in. All of it lives in
`theme.rs`; none of it is reprinted here.

- **Two font families, nothing else** — IBM Plex Sans for UI, IBM Plex Mono for branches, paths,
  diffs, terminal and code (`theme::font`, bundled by `crate::fonts`). Both are OFL.
- **Radii** are a scale in `theme::radius`, from the window corner down to a small mark. The step
  matters: at 5px across, the difference between `MARK` and `MARK_SM` is the difference
  between reading as a square and reading as a dot, and this app already uses a real circle for
  "an agent's status".
- **Band heights** are named in `theme::band`, **zone widths** in `theme::zone`. A new surface reuses
  an existing band height wherever it plausibly can, per rule 5.
- **Interface scale** applies to text size only (`theme::ui_scale`), deliberately not to padding,
  icons or fixed chrome. That module's docs enumerate exactly which surfaces honour it and why each
  exclusion exists.
