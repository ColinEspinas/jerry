# Handoff: Jerry — agent development environment

> **Updated 2026-07-29.** Sections rewritten in the latest pass: Keyboard affordances, Settings,
> Tab strip, Surface B (terminal), Surface C toolbar, Layout (title bar, status bar).
> If you have already built from an earlier copy, read `CHANGELOG.md` first and apply only the
> newest entry — it lists every delta with old → new values.

## Overview

Jerry is a desktop app for supervising several AI coding agents at once. Each **session** is one
agent, working in one git worktree, on one task. The UI is three fixed zones — session rail (left),
tabbed work surface (centre), files + changes (right) — and its whole job is triage: telling the
developer, at a glance, which of six-plus running sessions needs them.

Target implementation: **Rust + GPUI** (Zed's framework). The mockup was authored inside that
constraint — flat fills, 1px borders, small rounded corners, at most one solid shadow. There is no
blur, no layered transparency, no gradient, and no CSS-only animation anywhere in it.

## About the design files

`Jerry.dc.html` in this bundle is a **design reference written in HTML**, not production code and
not a component library. Do not port the markup. Read it for exact values (every colour, size and
spacing literal is inline) and rebuild each zone as GPUI elements using the codebase's own
conventions. Where this README and the HTML disagree, the HTML is authoritative — it is the artefact
that was reviewed.

Open it in a browser. The strip below the window switches the eight designed states; the session
rows, tabs, tree/changes toggle, Diff/File toggle and file stepper are all live.

## Fidelity

**High fidelity.** Final colours, type, spacing and copy. Rebuild pixel-close. The only elements
that are deliberately not final: the desk background and window drop shadow (mockup presentation
only — the real app gets OS window chrome), and the "STATES" strip under the window, which is
mockup scaffolding and must not be built.

---

## Layout

Window 1440 × 928 at the design size; every zone is fixed-height rows in a vertical flex column.

| Band | Height | Background | Bottom border |
|---|---|---|---|
| Title bar | 38 | `#101214` | `#1e2225` |
| Body (three zones) | fill | — | — |
| Status bar | 26 | `#101214` | top border `#1e2225` |

Zones inside the body, left to right:

| Zone | Width | Background | Border |
|---|---|---|---|
| Session rail | 276 (range 240–340) | `#101113` | right `#1e2225` |
| Work surface | fill (≈844) | `#131518` | — |
| Files / changes | 320 (260 in empty states) | `#101113` | left `#1e2225` |

The work surface stacks: tab strip 34 · session context bar 32 · optional conflict banner · surface
(fill) · surface footer 28.

---

## Zone 1 — session rail

The rail is the product. Everything else is secondary to answering "who needs me" in under a second.

### Grouping

Two modes, switchable from a `by urgency ▾ / by project ▾` control in the rail header (10px mono
`#8b9197`, hover bg `#1b1f22`).

**By urgency (default).** Sessions are grouped by status, groups ordered by urgency:
`Needs input → Failed → Review ready → Running → Idle`. Group header is 5×5 square in the status
colour + 9.5px/600 uppercase Plex Sans `#787f86`, letter-spacing .08em, + count in 9.5px mono
`#4a5057`. This ordering plus the coloured left edges is the whole "under a second" mechanism —
the answer is *how tall is the amber block at the top*, not read labels.

**By project → worktree.** Project header row (27 high): caret, project name 11px/500 mono
`#c2c7cc`, branch 10px mono `#4a5057`, then a right-aligned cluster of 5×5 status dots sorted by
urgency and a `7 wt` count. Children are worktree rows indented 16 with a 1px `#1e2225` vertical
spine. **Worktrees without a session appear here** — the `main` checkout (`checkout · clean`) and
leftovers like `wt/search-ratelimit` (`merged 11:04 · prunable`). That's the reason this mode exists.

### Session row

Padding `6px 10px 7px 12px`, left edge `2px solid <status colour>`, selected background `#1a1e21`.

1. **Agent badge** — 16×16, radius 3, agent tint background, agent initial 9px/500 mono.
2. **Title** — 12px/450/16 Plex Sans, ellipsised. `#dde2e7` selected · `#b8bfc6` normal · `#7d848b` idle.
3. **Meta** (right of title) — 9.5px mono. `#c99b4e` when waiting, else `#4e545a`.
4. **Second line** — 4px status dot, branch 10.5px mono (`#8b9197` selected / `#6b7178`), stat 10px mono right (`+38 −11`, or `3 failing` in `#c4726d`).
5. **Question preview** — waiting sessions only: 4/6 padding, radius 3, bg `#1c1710`, left border `2px #8a6420`, text 10.5px/15 Plex Sans `#c99b4e`. This is Jerry reading the tail of the agent's pty, and it is what lets triage finish without opening the session.

### Status vocabulary — use nowhere else

| Status | Label | Colour | Pill bg |
|---|---|---|---|
| ask | Needs input | `#e2a336` | `#3a2c14` |
| fail | Failed | `#e0625c` | `#3a1e1e` |
| review | Review ready | `#5cb87f` | `#1e3b2a` |
| run | Running | `#5a9ad4` | `#1e2f3e` |
| idle | Idle | `#565d64` | `#22262a` |

Colour is reserved for status and diffs. Nothing decorative is coloured.

### Rail chrome

Header 36 (`Sessions` 10px/600 uppercase `#6b7178`, grouping control, `+` with a ⌘N keycap pair) ·
filter row 30 (`/` + "filter sessions" `#4e545a`) · footer 28 (`8 worktrees · 2.1 GB` / `2 projects ·
11 worktrees`, and a `prune` affordance) — all 10px mono `#4a5057`, top border `#191c1f`.

---

## Zone 2 — work surface

### Tab strip (34)

Three peer tabs, each 13px horizontal padding, 7px gap, right border `#1c2023`. Active tab: bg
`#131518`, bottom border same as bg (so it merges into the surface); inactive bottom border `#1e2225`.
Label `#d3d8dd` active / `#767d84` inactive. A `×` at 11px mono `#4a5057`, then a `+` and a
⌘ / 1…8 keycap pair pinned right.

Each kind carries a 14×14 radius-3 chip (dimmed to bg `#1e2225` fg `#5e646a` when inactive):

- **agent CLI** — `❯` at 8px/500 mono, chip tinted with **that agent's** colour, so the tab is
  colour-coded to who is working. Label is the binary: `claude`, `codex`, `qwen`.
- **terminal** — pane glyph: 14×4 bar across the top plus a 5×2 radius-1 prompt mark at x3 y7, both
  in `#8b9197` on `#23272b`.
- **code** — the file's language chip, same table as the file tree (`rs` `to` `md` `sq`), so the tab
  always matches its file. Label is the file name in mono.

### Session context bar (32)

`#121417`, bottom border `#1c2023`. Agent badge 15px · agent name 11px/450 · 1px `#22262a` divider ·
branch 11px mono `#8b9197` · worktree path 10.5px mono `#4a5057` · spacer · status pill (19 high,
radius 3, 5px dot + 10px/500 label in the status colour) · `Merge` (outline) · `Archive` (ghost).

**Layout rule that matters:** every child is `flex: none` + no wrap; the worktree path is the single
flexible item and ellipsises. Without this the bar wraps and overflows when the centre narrows.

### Conflict banner

Shown when another session has touched a file on this branch. 7/12 padding, bg `#1b1610`, bottom
border `#33291a`. 5px amber dot · path 11px mono `#c99b4e` · one truncating sentence 11px Plex Sans
`#8a7548` · `Resolve now` button (bg `#3a2c14`, hover `#4a3818`, label `#e0b263`).

### Surface A — agent CLI (default tab)

**There is no chat UI.** The agent runs in a pty and Jerry renders its output verbatim.

- Header 27: `#111316`, `claude --resume 3d91e07` 10.5px mono `#8b9197`, `pid 48213` `#4a5057`, and
  right-aligned pty state — `attached · waiting on stdin` / `attached · streaming` / `exited 0` /
  `exited 101` / `detached · resumable` in 10px mono `#41464b`.
- Body: `#0d0f11`, 12/16 padding, lines at **12px/19 mono, `white-space: pre`**. Palette:
  prompt `#8fbde6`, body `#a7adb4`, dim `#6b7178`, ok `#6ab97f`, error `#e0625c`, amber `#d8a94a`,
  heading `#ced4da`, activity `#5a9ad4`, selected menu row fg `#e0b263` on bg `#1f1a10`.
  The agent's question is *its own* numbered prompt (`❯ 1. Squash into one`), never a designed card.
- Block cursor 7×15 `#5a9ad4` after a `❯ ` prompt in `#5cb87f`, shown when the process is not
  streaming and not waiting on its own menu.
- Footer strip 8/12, `#111316`, top border `#1c2023`: the word `Jerry` (9px/600 uppercase `#454b51`)
  then git-level actions the CLI cannot own, by status —
  review: `Keep all ⌘⏎` (green) · `Review diff` · `Open in editor` · `Discard worktree`;
  ask: `Open terminal` · `Interrupt ⌃C`; fail: `Retry ⌘R` · `Open terminal` · `Discard worktree`;
  run: `Interrupt ⌃C` · `Open terminal`; idle: `Resume ⌘⏎` (blue) · `Archive`.
  Right-aligned hint in 10px mono `#41464b`. Keeping this boundary visible is deliberate.

### Surface B — terminal

Same geometry as the CLI pane; header shows `zsh` + worktree path + `⌘D split · ⌘K clear`.

### Surface C — code (Diff | File)

One tab, two views, toggled in its own toolbar (31 high, `#121417`, bottom border `#1c2023`):

`dir` 10.5px mono `#4e545a` · `name` 11.5px/450 mono `#c8cdd2` · optional tag pill (`new` `#7fc79a`
on `#1e3b2a`, `del` `#d18b86` on `#3a1e1e`, `conflict` `#e0b263` on `#3a2c14`) · `+142` `#5f9c78` ·
`−8` `#b06a66` · spacer · `‹ 3 of 12 ›` stepper (rendered only when >1 file) · segmented
`Diff | File` (track `#171a1d`, active `#242a2f`/`#d3d8dd`) · 1px divider · `Accept file ⏎`.

**Accept file is always rendered**, dimmed (`#454b51` / border `#1f2327`) when there is nothing to
accept. It must never appear or disappear with the view — that reflows the toggle under the cursor.

**Diff view.** Unified, full centre width: two 52px right-aligned line-number gutters (`#3a3f44`),
a 16px sign column, then code at 12.5px/20 mono, `white-space: pre`.

| Line kind | Background | Text | Sign |
|---|---|---|---|
| context | none | `#868d94` | — |
| added | `#12211a` | `#9fd0b2` | `+` `#4e8c68` |
| deleted | `#211517` | `#d6a4a0` | `−` `#a35f5b` |
| hunk header | `#15181c` | `#5f666e` | — |
| fold | `#121417` | `#4a5057` | `⋯` |

Folds (`⋯ 24 unchanged lines`) are how a 12-file change stops being a wall: only changed hunks with
3 lines of context, everything else collapsed. Files with no hunk loaded show
`⋯ 48 changed lines — press ⏎ to load this hunk`. Footer 28: `j/k hunk · ⏎ accept file · ⌘⏎ keep
all · ] next file` plus `3 reviewed` right.

**File view.** Breadcrumb 26 (`src › db › query_builder.rs › impl QueryBuilder › build`, 10.5px mono,
separators `#3d4248`, active crumb `#a9b0b7`) with error/warning counts right. Code at 12.5px/20 mono:
52px line numbers, then a **3px git gutter** (`#2c6244` for agent-touched lines, transparent
otherwise), then code with syntax colours — keyword `#b477cf`, function `#74ade8`, type `#dfc184`,
literal/`self` `#bf956a`, comment `#5d636f`, punctuation/text `#acb2be`. Current line bg `#181c20`,
caret 2×15 `#5a9ad4`. Status bar 28: `rust-analyzer` + green dot + `indexed 1,284 crates` … `Rust`,
`ln 44, col 14`, `LF`.

**Language server UI** — the differentiator; three states, one at a time (the mockup exposes a
switcher for review; the real app shows whichever the editor is in):

- *Completions* — popup anchored under the caret line, 590 wide, border `#2b3238`, bg `#181c20`,
  radius 5, one shadow `0 8px 20px rgba(0,0,0,.5)`. Left 290: rows 22 high, 13×13 kind chip
  (fn `#8fbde6` on `#243c50`, var `#d8a94a` on `#33280f`, type `#c294e0` on `#33203e`), label
  11.5px mono, detail right 10px `#4e545a`; selected row bg `#243c50`, label `#e3e8ed`; footer
  `⇅ move · ⏎ accept · ⇥ snippet`. Right 300: signature in mono, doc in 11px Plex Sans `#7d848b`,
  module path footer.
- *Diagnostic* — 2px dotted `#e0625c` under the offending span, dim inline message at end of line
  (`#6b4a48`), row tinted `#191416`, and a card below: message `#e3908b`, note `#7d848b`,
  `rust-analyzer · E0277` + `quick fix: wrap in Column::from ⌘.`
- *Hover* — 1px `#4d7ba8` underline on the hovered symbol, card 430 wide: signature, doc prose,
  `core::convert` + `F12 definition` footer.

### Surface D — merge conflict

Reached from the banner. Header: `Resolve merge` 12.5px/500 · path · `hunk 1 of 2` · right
`⌥← take left · ⌥→ take right · ⌥↑ take both`. Then a green pre-flight strip (`#151a17`):
`Jerry can auto-resolve 2 of 3 files — the edits don't overlap. Only this file needs you.`

Two equal columns split by 1px `#1e2225`. **Each side is headed by its session, not by ours/theirs**
— agent badge, agent name, branch, commit count on `#151a20`. Code at 11.5px/19 mono with 40px
gutters; side A's own lines tint `#231c0f`/`#d8bd85`, side B's `#152218`/`#9fd0b2` (the same amber and
green as those agents). Each column footer carries its `Take left` / `Take right`.

Result strip at the bottom: `Result` label, `both edits kept · layer order preserved`, the merged
preview using both tints, and `Take both ⌥↑` as the green primary — Jerry proposes the answer.

### Empty states — two different screens

*First run:* 560-wide panel. Title `Start your first session` 13.5px/500, subtitle "A session is one
agent, one worktree, one task. Nothing you start here touches your checkout." Then a bordered
composer (`#161a1d`, border `#24292e`, radius 6): a row of repo / base / agent fields separated by
1px `#22272b`, a prompt area, and a footer with `worktree: ~/.jerry/wt/<branch>` and
`Start session ⌘⏎`. Below, three hint rows: keycap + 11.5px Plex Sans `#6b7178`.

*Everyday empty:* same composer, title `Nothing running`, subtitle about the merged branches, only
two hints, and the rail switches to a dim `Merged today` list plus `8 worktrees pruned`. Right panel
shows a single centred `no worktree focused` in 10.5px mono `#3d4248`.

No illustration, no mascot, no onboarding art in either.

---

## Zone 3 — files and changes

Header 36: segmented `Files | Changes` (**Files is first and default**; the review state opens
straight into Changes), then `+95` / `−11` totals in mono.

**Files (tree).** Rows 22 high, indent 13 per level, 11.5px mono. Folder icon is two rects — a 5×3
tab at (0,1) and a 12×8 radius-2 body at (0,3) — outlined `#4e545a` when collapsed, filled `#23272b`
with a `#6b7178` border when open. Files get a 13×13 radius-2.5 language chip:

| Ext | Chip | Fg | Bg |
|---|---|---|---|
| `.rs` | `rs` | `#c0824a` | `#2e2113` |
| `.toml` | `to` | `#8b9197` | `#23272b` |
| `.md` | `md` | `#7f9ad4` | `#1d2532` |
| `.sql` | `sq` | `#6ab97f` | `#1b2a20` |

Changed files carry an `A` (`#5f9c78`) or `M` (`#a3873f`) mark at the right. Selected row bg `#1a1e21`.

**Changes.** Header 7/12 on `#121417`: file count + a 56×3 review progress bar (`#5cb87f` on
`#22262a`) + `3 reviewed`. Rows are 27 high, bottom border `#171a1c`, left edge `2px` `#3f5b74`
when selected, bg `#1a1e21`: a 12×12 review checkbox (checked border `#2f6d4b`, bg `#24503a`, `✓`
`#9fdcb6`), dir in `#4e545a`, name in `#c2c7cc` (dimmed `#7d848b` once reviewed), tag pill,
`+n` / `−n`, and a five-segment 3×8 stat bar (`#4e8c68` / `#a35f5b` / `#22262a`). **Clicking a row
opens that file's diff in the centre** — the panel never shows a diff itself. Footer 29:
`click a file to open its diff in the centre · ] next file`.

---

## Keyboard affordances

Shortcuts render as **keycaps**, never bare glyph runs: 15 high, min-width 15, padding 0 4,
radius 3, bg `#181c1f`, border 1px `#272c31`, 9.5px/450 mono `#7d848b`. One cap per key
(`⌘` `K`), then the label in 10.5px/450 Plex Sans `#6b7178`. Inside a coloured button the cap goes
transparent and borrows the button's tint (green `#376b4d`/`#8ac9a4`, blue `#365b78`/`#8fbde6`).

Bound in the design: `⌘N` new session · `⌘1..8` focus session n · `⌘K` commands · `⌘⇧K` sessions ·
`⌘⏎` keep all / start / resume · `⌃C` interrupt · `⏎` accept file · `j/k` hunk · `]` next file ·
`⌥←/→/↑` take left/right/both · `⌘.` quick fix · `F12` definition · `⌘D` split · `⇥` snippet.

---

## State model

```
Session { id, agent, agent_tint, cli_binary, pid, sha, branch, worktree_path,
          title, status, question: Option<String>, meta, add, del, files, reviewed }
Status  = Ask | Fail | Review | Run | Idle
Project { name, base_branch, worktrees: Vec<Worktree> }
Worktree{ path, branch, session: Option<SessionId>, note }   // note: "clean" | "merged … · prunable"
```

UI state, all of it observed in the mockup:

| Field | Values | Notes |
|---|---|---|
| `rail_mode` | `Urgency` \| `Project` | persists per window |
| `focused_session` | `Option<SessionId>` | drives centre + right |
| `tab` | `Cli` \| `Terminal` \| `Code` | three peers |
| `code_view` | `Diff` \| `File` | forced to `File` when the session has no changes |
| `open_change` | `Option<FileKey>` | defaults to first changed file; `‹ ›` and `]` step it |
| `right_pane` | `Files` \| `Changes` | `Files` default; review state opens `Changes` |
| `lsp_popup` | `None` \| `Completions` \| `Diagnostic` \| `Hover` | one at a time |

Transitions worth copying exactly: selecting a session keeps the current tab if it is Terminal or
Code, otherwise falls back to the CLI tab, and resets `open_change`; clicking a change row sets
`tab = Code, code_view = Diff, open_change = row`; the conflict banner's `Resolve now` swaps the
centre surface.

## Design tokens

Fonts: **IBM Plex Sans** (UI, weights 400/450/500/600) and **IBM Plex Mono** (branches, paths,
diffs, terminal, code, weights 400/450/500). Nothing else. Both are OFL — bundle them.

Type scale actually used: 9, 9.5, 10, 10.5, 11, 11.5, 12, 12.5, 13.5. Line heights: 15, 16, 17, 18,
19, 20. Uppercase labels carry .075–.09em tracking.

Radii: 10 window · 5–6 cards and panels · 4 buttons · 3 chips and keycaps · 2–2.5 small marks.

Shadows: **one** in the whole product — the completion popup, `0 8px 20px rgba(0,0,0,.5)`. If GPUI
makes that awkward, drop it and keep the border. The window shadow in the mockup is desk dressing.

Spacing rhythm: 2 · 3 · 4 · 5 · 6 · 7 · 8 · 9 · 10 · 12 · 14 · 16 · 22. Row heights: 19 · 20 · 21 ·
22 · 23 · 26 · 27 · 30 · 31 · 32 · 34 · 36 · 38.

`tokens.rs` in this folder lists every colour as a Rust constant, grouped the same way as above.

## Assets

None. Every icon is composed from rects and text glyphs (`❯ ⎿ ● ▾ ▸ ‹ › ⋯ ✓ ✗ ⏸ ⠸ ×`) precisely so
that nothing needs an SVG pipeline. Keep it that way if you can — it is also why the whole UI ports
cheaply.

Content note: every path, branch, agent name, diff and terminal transcript in the mockup is
plausible sample data written for review. Replace it with real state; do not ship the strings.


---

## Command palette (⌘K)

Overlay, not a page. Scrim rgba(6,7,8,.62) covering everything below the title bar (a flat alpha
fill — **not** a blur), panel 684 wide pinned 64 from the top: bg `#15181b`, border `#2b3238`,
radius 8, shadow 0 12px 34px rgba(0,0,0,.55). Clicking the scrim or esc dismisses.

**Input row 44.** Scope prefix glyph 12px mono `#5f7f9e` (› commands, @ files), query at 13px
Plex Sans (`#dde2e7`, placeholder `#4e545a` reading "Type a command, file or session…"), a 1.5×16
caret `#5a9ad4`, then a segmented scope control on the right: All ⇥ / Commands › / Files @
(track `#171a1d`, active `#242a2f`). Scope is reachable both by clicking and by typing the prefix.

**Results.** Grouped, header 9.5px/600 uppercase `#5b6167` + count. Rows 30 high, selected row
bg `#1a1e21` with a 2px `#3f5b74` left edge:

- 15×15 radius-3 kind chip — commands › in `#7f9ad4` on `#1d2532`; files the language chip;
  sessions the agent badge (so the palette inherits the rail's colour coding).
- Label at 12px Plex Sans (commands, sessions) or 11.5px mono (files), with the **matched substring
  in `#8fbde6`** — three spans, pre/match/post.
- Secondary in 10.5px mono `#5e646a`: branch for sessions, `src/db/ · +142 −8` for files, a one-line
  description for commands.
- Optional status dot, then a keycap for the bound shortcut.

Empty query shows Sessions (⌘1..8), Commands and Recent files together — the palette is also the
session switcher. Footer 29: ↑↓ move · ⏎ run · ⇥ next scope · esc close, plus the result count.

## Settings

A separate surface, not a modal: it replaces the three zones while the title bar and status bar stay.
esc (rendered as a keycap in the nav header) returns to the workspace.

**Nav 212 wide**, `#101113`, right border `#1e2225`. Groups (Workspace, Editor, Other) with the
same 9.5px uppercase header as the rail. Rows 25 high, indent 10, active row bg `#1a1e21` + 2px
`#3f5b74` left edge, label 11.5px Plex Sans (`#dde2e7` active / `#8b9197`), optional count badge in
9.5px mono `#454b51`. Footer: jerry 0.4.2 · settings.toml.

Pages: General · **Agents** · **Worktrees** · Keymap · Editor · Language servers · Theme ·
Notifications · Integrations · About. Agents and Worktrees are designed; the rest are nav-only in
this mockup.

**Content column.** Header block 18/26/14 with the page title at 15px/500 `#dde2e7` and a one-line
rationale at 11.5px/17 `#767d84`, bottom border `#1c2023`. Then sections, each introduced by a
9.5px uppercase label.

*Agents › Installed* — bordered card (`#23282c`, radius 6) of four rows on `#161a1d`, separated by
`#1f2327`: 18×18 agent badge · name 12px (104 wide) · binary path 10.5px mono `#6b7178` (172 wide) ·
model 10.5px mono `#8b9197` (flexes) · a `default` pill (`#7fc79a` on `#1e3b2a`) · green dot +
"ready" · Edit. Card footer on `#131619`: "+ Add an agent — any binary that speaks a resumable
session on stdin". That sentence is the product position; keep it.

*Worktrees › Disk* — same card shape: status dot · worktree path 10.5px mono (196 wide) · branch ·
size · a right-aligned Open (`#6b7178`) or Prune (`#c4726d`). Footer totals
"11 worktrees · 2.1 GB · 1.4 GB saved by hardlinked target/" and a Prune 1 merged action.

*Settings rows* — one pattern for every scalar setting: 11px vertical padding, bottom border
`#1c2023`, label 12px `#c8cdd2` + hint 11px/16 `#6b7178` on the left, control right:

- **toggle** — 26×15 track (radius 8, on `#2f6d4b` / off `#23272b`) with an 11px knob
  (on `#c8ecd6` / off `#6b7178`), knob justified to the end when on.
- **stepper** — − / value / +, the two buttons 19×19 radius 3 with a `#2a2f34` border, value
  11.5px mono `#c8cdd2` in a 46-wide centred slot.
- **path** — value in 10.5px mono `#a9b0b7` plus a Change… outline button.

Hints carry the reasoning, not just the label ("Past eight the rail stops being glanceable",
"Costs a cold rebuild when the toolchain changes") — that tone is part of the design.

Added state: palette_open, palette_scope (All | Commands | Files), settings_open, settings_page,
and the settings values themselves.

## Files

- `Jerry.dc.html` — the reviewed mockup, eight states, interactive.
- `tokens.rs` — colour constants transcribed from it.
