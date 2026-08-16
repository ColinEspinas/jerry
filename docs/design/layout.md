# Window layout

**Code:** `crates/app/src/root/`, `crates/app/src/title_bar/`, `crates/app/src/status_bar/`
**Tokens:** `theme::{band, zone, surface, border}`

## What it's for

Jerry is one window with a fixed skeleton: a title bar, three side-by-side zones, and a status bar.
The skeleton never changes shape. Surfaces swap *inside* Zone 2; panels collapse and resize; but a
developer who has used the app for a week always knows which third of the window answers which
question. That predictability is what makes triage fast, and it is why nothing here floats, docks
elsewhere, or becomes a modal.

## Structure

### Bands, top to bottom

| Band | Height | Notes |
|---|---|---|
| Title bar | `theme::band::TITLE_BAR` | Platform-dependent — see below |
| Body — three zones | fill | |
| Status bar | `theme::band::STATUS_BAR` | Three type tiers; see below |

### The three zones

| Zone | Width | Owns |
|---|---|---|
| 1 — agent rail | `theme::zone::RAIL_WIDTH`, resizable | Which agents exist and which needs you ([`rail.md`](./rail.md)) |
| 2 — work surface | fill | Whatever you are looking at right now ([`work-surface.md`](./work-surface.md)) |
| 3 — files / search / changes | `theme::zone::PANEL_WIDTH` | Navigating this worktree ([`sidebar.md`](./sidebar.md)) |

Zones 1 and 3 are drag-resizable. The clamp is pure and window-free, in `root::layout`
(`RAIL_DEFAULT`/`RAIL_MIN`/`RAIL_MAX` and the panel's equivalents), so the arithmetic is unit-tested
without a live GPUI window; `root::resize` owns the actual drag. Widths are computed from the drag's
absolute cursor position each tick rather than from an armed drag-start baseline — which is what
removes the "drag state left dangling if the mouse is released outside the window" class of bug by
construction.

### The work-surface stack

Zone 2 is itself a vertical stack, and every layer has a named band height:

```
tab strip              theme::band::CHROME_HEADER
agent context bar      theme::band::CONTEXT_BAR
(conflict banner)      only when another agent has touched a file on this branch
surface                fill  — the agent pane, a terminal, a file, the graph, a review, a run
surface footer         theme::band::SURFACE_FOOTER  or  ::PTY_INFO_FOOTER
```

A pane gets exactly **one** bottom bar, chosen by its `ProcessKind`, never both stacked: the agent
pane's readout strip (`SURFACE_FOOTER`) or the shell pane's info footer (`PTY_INFO_FOOTER`).

### One header rule across three columns

The work-surface tab strip, the rail's own sidebar strip, and the Zone 3 panel header all sit on one
y and all read `theme::band::CHROME_HEADER`. This is the canonical instance of
[`principles.md`](./principles.md) rule 5, and that constant's doc comment records what happened
when the three drifted: a visible staircase at every column boundary. Their bottom rule shares one
colour too (`theme::border::RAIL_INNER`), for the same reason.

**Changing any of the three heights changes a line that spans the whole window. Check the other two
in the same edit.**

### Title bar — two variants

Platform-dependent, driven by one setting (`keymap::WindowControlsStyle`) that also decides which
glyphs keycaps resolve to. See `keymap.rs`'s own docs for why the two are deliberately one setting
and not two.

- **macOS** — the OS paints its own traffic lights over the window; `title_bar::render` reserves
  their cluster width and draws the trailing divider clear of them.
- **Windows / Linux** — a menu row (`File Edit View Agent Help`, whose dropdowns live in
  `title_bar::menu`) and three caption buttons pinned to the right edge, outside the band's own
  padding. The close glyph is two rotated rects; `title_bar::render`'s
  `CLOSE_GLYPH_HALF_DIAGONAL` is the geometry.

Everything between the two ends — project chip, the compact urgency dot chips, panel toggles — is
shared.

The override is a **cosmetic preview**, not a rebinding: GPUI resolves `"secondary"` to a physical
key once, at compile time, so previewing the macOS look on Linux renders `⌘P` while Ctrl+P is still
the key that works. That mismatch is accepted, and confined to an explicitly opt-in action whose own
label says "preview". `keymap.rs`'s docs carry the full argument.

### Status bar

**The bar watches agents. It is not a text editor's footer.** That is the whole editorial rule, and
it was arrived at subtractively: an audit of the previous bar found eight of its thirteen readouts
lifted straight from VS Code — cursor position, indent width, line ending, encoding, zoom and UI
scale among them — answering *what am I typing into* rather than *what are my agents doing*. All
eight were deleted, code paths included, in the rev-6 rebuild (issue #293).

What is left is three groups, one per type tier, separated by a deliberately heavier divider
(`theme::status_bar::DIVIDER`; at the old weight the groups they separated ran together):

- **Left** — a transient notice slot (an available update from `updater`, or one-off
  keep-all/discard feedback from `worktree_history::flow`), the branch cluster with ahead/behind,
  `N agents running`, and the machine's real CPU/memory load.
- **Right** — the environment chip and the palette keycap hint. That is all of it.

Resource figures are real measurements sampled off the OS (`status_bar::process_stats`,
`status_bar::resources`), not decoration.

Two removals are worth knowing about because they look like omissions and are not: the **urgency
dot cluster** moved into the title bar's compact dot chips, and **`N worktrees · Y GB`** was dropped
because the rail footer already carries it 30px away — the rail owns worktree inventory, the bar
owns activity. Editor zoom survives as `mod+plus`/`mod+minus`; only its readout is gone.

The status bar is rendered as an unconditional sibling of the workspace/Settings swap, so it stays
visible while Settings is open — which is why transient feedback lives here rather than in the rail
footer, where it would vanish exactly when a long operation needed reporting.

## Rules that matter

- **Nothing floats.** Overlays are the command palette, the completion popup, and menus — all of
  which are dismissible and none of which is a place you can leave the app parked. Settings replaces
  the three zones rather than opening as a modal; the title bar and status bar stay.
- **Overlay focus is centrally owned.** `root::focus` and `OverlayFocus` decide who has the keyboard
  when a palette or popup is up. A new overlay routes through it rather than grabbing focus itself.
- **Band heights come from `theme::band`.** A surface that needs a new band height adds a named
  constant with a doc comment saying what it is, rather than a literal at the call site — otherwise
  rule 5 above has nothing to enforce.
- **Two states distinguished anywhere in the app are never summed anywhere in it.** This is why the
  status bar counts *running* agents rather than every open agent: the rail spends its whole design
  on separating five statuses, and a bar that adds them back into one number contradicts it.
- **One fact has one home.** If two chrome surfaces would show the same number, the one that owns
  the action on it keeps it. Worktree inventory belongs to the rail because the rail has the prune
  button.
- **Every user-visible count goes through `root::plural`.** `plural::count(n, "file", None)`,
  `plural::form(n, "needs", "need")`. Zero is plural in English and the helper knows that. This is
  binding on new code and is stated in [`CLAUDE.md`](../../CLAUDE.md) too.
- **Text scaling is opt-in per surface.** `theme::ui_scale::scaled_px` is applied where its module
  docs say it is; the code surface and terminal panes are deliberately excluded because they own
  their own font-size settings and a second multiplier would compound with them.

## Not built yet

- **Interface scale does not reach padding, icons or fixed chrome.** Retrofitting every literal
  `Pixels` constant in `theme.rs` to scale is out of scope for the current mechanism, and
  `theme::ui_scale`'s docs say so explicitly rather than implying full coverage.
- **Zone 2 is single-pane.** There is no split view within the work surface — tabs replace each
  other in one region. A vertical/horizontal split would be a real design decision, not an
  incremental change, and belongs in [`decisions.md`](./decisions.md) first.
