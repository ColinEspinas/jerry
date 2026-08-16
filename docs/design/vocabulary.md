# Visual vocabulary

**Code:** `crates/app/src/{theme.rs, icons.rs, keymap.rs, language.rs}`, `crates/app/src/rail/status.rs`, `crates/app/src/root/widgets.rs`
**Tokens:** `theme::{status, agent, lang, tag, button, radius, band}`

## What it's for

The small set of shapes Jerry reuses everywhere. Every one of them is defined once and drawn from
one place, which is what lets a status colour mean the same thing in the rail, the tab strip, the
status bar and the palette without any of those four re-deciding it.

If you are adding a surface, this is the page to read first: almost everything a new surface needs
already exists here, and reaching for a new shape needs a reason.

## Structure

### Status — five values, used nowhere else

`rail::status::Status` is the whole vocabulary: `Ask`, `Fail`, `Review`, `Run`, `Idle`. It carries
its own rendered label (`Status::label`), its urgency ordering (`Status::urgency_rank`,
`Status::ORDER`), its dot/edge colour (`Status::color` → `theme::status`) and its pill background
(`Status::pill_bg`).

| Variant | Rendered label | Meaning |
|---|---|---|
| `Ask` | Needs input | Agent has been quiet past `AGENT_ASK_IDLE_THRESHOLD`, or a hook says it's blocked |
| `Fail` | Failed | Non-zero exit, or killed by a signal |
| `Review` | Finished | Exited 0 with a real, non-empty diff against base |
| `Run` | Running | Alive and producing output, or alive and briefly paused |
| `Idle` | Idle | No process running, or a shell just sitting there |

The variant name and the rendered word are deliberately allowed to differ — `Review` renders as
`Finished`. See [`decisions.md`](./decisions.md) §10 for why.

Anywhere a status is shown as a **pill**, it is the dot plus the label in `Status::color` on
`Status::pill_bg`. Anywhere it is shown as a **row edge** or a **dot**, it is `Status::color` alone.
There is no third treatment.

### Agent tints

`theme::agent` holds one `(fg, bg)` pair per agent, and `theme::agent::TINT_POOL` is the enumerable
list. An agent's tint is its identity across the whole window: the 16px badge in a rail row, the tab
chip on its CLI tab, the badge in the agent context bar, the session row in the palette, and its own
side of a merge-conflict view all read the same pair through
`work_surface::state::agent_tint`.

The pool obeys a **reserved-hue rule** recorded in that module's own docs: no agent tint may sit in
a hue already spent on status or on diffs. That rule is enforced by a real test
(`theme::agent_tint_allocation_tests`), and forcing a reallocation is what produced today's copper /
teal / periwinkle / steel-blue set — see [`decisions.md`](./decisions.md) §9.

### Chips

A chip is a small rounded square (`theme::radius::CHIP`) carrying a two-or-three-letter label or a
glyph. Four kinds exist, and no surface invents a fifth:

- **Agent chip** — the agent tint pair, agent initial. Identity.
- **Language chip** — derived from the file extension, *never* hand-assigned. One table
  (`language::EXTENSIONS`, colours in `theme::lang`) feeds the file tree, the code tab, the palette's
  file results, and the Settings language-server rows. A file's chip is the same object in all four.
- **Tab chip** — `work_surface::state::TabChipKind`, either the CLI `❯` or the terminal pane glyph,
  tinted with the agent's own colour when active and dimmed to `theme::border::ZONE` /
  `theme::text::FAINTER` when not (`tab_chip_colors`, `file_tab_chip_colors`). The dimmed pair is
  shared, so an inactive agent tab and an inactive file tab dim identically.
- **Tag pill** — `theme::tag`, for a file's `new` / `del` / `conflict` state.

### Keycaps

Shortcuts render as keycaps, never as bare glyph runs. One cap per resolved key, in a row.

Two sizes, both in `root::widgets::render_keycap_sized` behind `KeycapSize`: `Standard`
(`theme::band::KEYCAP`) for a control's own binding, and `Hint` (`theme::band::KEYCAP_HINT`) for the
footer/hint rows that list several. Inside a coloured button the cap goes transparent and borrows the
button's tint (`theme::button::*_KEYCAP`).

**Every binding in the product is authored as a spec string** — `"mod+shift+K"`, `"ctrl+C"` — and
rendered through `keymap::resolve_combo`, which maps the eight recognised tokens
(`mod alt ctrl shift enter esc tab bksp`) onto the macOS or the Windows/Linux table. There is no
literal `⌘` in calling code anywhere. The Settings › Keybindings page goes one better and resolves
straight off the live registered `gpui::KeyBinding`s via `keymap::resolve_keystroke`, so it cannot
drift from what is actually bound.

### Icons

Two populations, and the split is deliberate.

**Shipped icons** are real [Phosphor](https://phosphoricons.com) SVGs (MIT) vendored under
`assets/icons/`, drawn through `icons::Icon` — twelve mapped slots, covering *actions and views only*
(panel tabs, sidebar strip cells, the overflow menu, the terminal tab, the prune button).

**Jerry's own vocabulary stays hand-drawn**: agent tint chips, status dots, file-extension chips,
diff gutter marks, and the graph's lanes and merge elbows. Those are shapes that carry meaning
defined on this page, not generic action glyphs, and a third-party icon family has no opinion about
them.

Two rules the `icons` module enforces mechanically rather than by convention:

- **One optical box per row, never one size per icon.** The only way to draw an icon is through an
  `IconRow`, constructed once with one `IconSize`; a lone icon is a row of one. Every vendored file
  is on the same `0 0 256 256` canvas, asserted by a test, so equal boxes really do give equal
  optical weight.
- **`bold` below 20px.** `regular`'s stroke reads thin against Jerry's greys. `weight_for_size` is
  that rule as code, paired against what `assets/icons/` actually holds by a test.

A user-supplied **icon pack** (`icon_pack.rs`) can override a slot with a real file, rendered through
`gpui::img` so it keeps its own colours — as opposed to the shipped Phosphor icons, which go through
`gpui::svg` precisely so they tint like text.

## Rules that matter

- **The status vocabulary is closed.** A new UI state is one of the five, or it is not a status.
  Adding a sixth changes the rail's group order, the status bar's counters and the urgency ranking
  simultaneously — it is a `decisions.md` entry, not a local change.
- **Language chips are derived, never assigned.** Adding a language means adding a row to
  `language::EXTENSIONS` and its colours to `theme::lang`; every surface then picks it up for free.
- **No literal shortcut glyph in calling code.** Author the spec string. A hardcoded `⌘` is a bug on
  Windows and Linux.
- **A new icon slot means vendoring the file and extending `Icon`**, not drawing a shape inline. The
  reverse is also true: something in the hand-drawn list above does not become a Phosphor icon
  without an entry in `decisions.md`.

## Not built yet

- **Icon packs cover one slot family.** `icon_pack.rs` is wired only to the three process-kind chips
  (Claude / Codex / Shell). The other ~25 icon concepts fall back to the built-in rendering. Stated
  follow-up in that module's own docs.
- **Interface scale is text-only.** `theme::ui_scale` scales font sizes on the surfaces its docs
  enumerate; padding, chips, badges, keycaps and fixed chrome do not scale, and the code surface and
  terminal have their own separate font-size mechanisms instead.
