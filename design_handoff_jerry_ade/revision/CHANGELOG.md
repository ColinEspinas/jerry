# Changelog — Jerry design handoff

Newest first. Each entry is a **delta against the previous handoff**: apply only what is listed
here. The README in this folder is always the current full spec — when an entry says a README
section was rewritten, re-read that section; otherwise don't.

Tell the agent literally: *"Read design/jerry/CHANGELOG.md and apply the 2026-07-29 entry."*

---

## 2026-07-29 — platform chrome, settings, tabs, scaling

Ten changes. Nothing in the session rail, diff engine, conflict resolver or palette result rows
changed — leave those alone.

### 1. Title bar is platform-dependent (new)

README §Layout, §Design tokens.

- Two variants of the 38-high title bar. **macOS:** the existing three 11px `#3a3f44` dots, then a
  1px `#22262a` divider. **Windows / Linux:** no dots; a menu row instead (`File Edit View Session
  Help`, 22-high items, 8px padding, radius 3, 11px/450 `#8b9197`, hover `#1b1f22`), and three
  caption buttons pinned to the right edge, 44 wide × full band: minimise (10×1 rect), maximise
  (9×9 1px outline), close (two 11×1 rects rotated ±45°), glyphs `#a9b0b7`, close hover bg
  `#8c3a38`, others `#1b1f22`. Caption buttons sit **outside** the 12px band padding.
- Default follows the OS. Overridable — see change 3.
- Everything between (project chip, urgency counters, panel toggles) is unchanged and shared.

### 2. Shortcuts resolve through one OS keymap (behavioural)

README §Keyboard affordances — rewritten.

- One table, two columns: `mod alt ctrl shift enter esc tab bksp` → macOS `⌘ ⌥ ⌃ ⇧ ⏎ esc ⇥ ⌫`,
  Windows/Linux `Ctrl Alt Ctrl Shift Enter Esc Tab Bksp`. Every binding in the product is authored
  as a spec string (`mod+shift+K`) and rendered through this map — no literal `⌘` anywhere.
- Each resolved part gets **its own keycap**; a combo is a 3px-gap row of them. Two sizes:
  standard 15-high (`#181c1f` / border `#272c31` / 9.5px `#7d848b`) and **new** hint size 14-high,
  padding 0 3.5, bg `#15181a`, border `#23272b`, 9px `#6b7178`.
- All bare glyph runs are gone. Converted: diff footer, terminal header, palette footer,
  completion footer, hover-card footer, quick-fix chip, conflict header and its three take-buttons,
  change-list footer, empty-state hint list, every action button, rail `+`, status bar. Hint rows
  are now `[keycaps] label` pairs, 11px gap between pairs, label 10px Plex **Sans** `#4a5057`
  (was 10px mono, one run).

### 3. Settings — narrower, config-file-first, five new pages

README §Settings — rewritten.

- **Width:** content column capped at **700px** (header and body share the cap), left-aligned
  inside the existing 26px padding. Nav stays 212.
- **Config banner** on every page, directly under the header: 1px `#23282c` / radius 6 /
  `#161a1d`, 7/10 padding — `to` chip, `~/.config/jerry/settings.toml` in 10.5px/450 mono
  `#a9b0b7`, the page's key list in 9.5px mono `#454b51` (ellipsised), a `TOML | JSON` segment
  (switches the path and the snippet below), and an `Open file` outline button.
- **Snippet block** at the foot of each designed page: section header `In settings.toml`, then the
  page's real keys in 11px/18 mono inside 1px `#23282c` / `#111417`, section lines `#c294e0`,
  key lines `#a9b0b7`, comments `#4e545a`. Caption under it: the file is the source of truth,
  this panel is a view of it.
- **Nav regrouped:** Workspace (General, Agents 4, Worktrees 11) · Interface (Appearance & scaling,
  Themes 6, Keybindings 48) · Editor (Editor, Language servers 5) · Other (Notifications,
  Integrations, About). Default page is now **General** (was Agents).
- **New page — General:** `Window controls` as a segmented choice `System | macOS | Windows`
  wired live to change 1 · `Default environment` path row (`WSL · Ubuntu-24.04`) · restore
  sessions · confirm before discarding a worktree.
- **New page — Appearance & scaling:** four preview cards (90/100/110/125%) each showing a session
  title, branch and a keycap pair rendered at that scale; selected card border `#3f5b74`, bg
  `#161b1f`. Then rows: interface scale (choice), editor font size 13px (stepper), terminal font
  size 12.5px, follow system text size, zoom per editor tab.
- **New page — Themes:** 212-wide cards, 34-high five-swatch strip over a name row; six themes
  (Jerry Dark default, Jerry Dim, Slate, Ember, Moss, Paper light/beta); selected border
  `#3f5b74`, `in use` mark 9px `#7fc79a`. Rows: follow system appearance, high-contrast diff.
- **New page — Keybindings:** filter row (`/ filter 48 bindings`, right-aligned count), then rows
  of command 11.5px `#c2c7cc` · context 64-wide 10px mono `#5e646a` · **keycaps right-aligned in a
  96 column** · source 36-wide, `base` `#5e646a` / `user` `#8fbde6`. Includes `Undo last action`
  and `Redo` — the undo stack is a first-class, listable command, not a hidden gesture.
- **New page — Language servers:** one row per language — ext chip · language 78-wide · server
  binary + version 196-wide mono · note · status dot + word · `Logs` / `Install` action. Rust,
  TypeScript, Vue, Python ready; Go `not installed` (`#565d64` dot, blue `Install`). Rows below:
  format on save, inlay hints, diagnostics in the rail.
- **New page — Editor:** indentation stepper, soft wrap, show whitespace.
- **New row control — `choice`:** a segmented control matching the Diff/File toggle (2px track
  `#171a1d`, 19-high options, active `#242a2f` / `#d3d8dd`). Fourth kind alongside toggle,
  stepper and path.

### 4. Tab strip is a real tab list

README §Zone 2 → Tab strip — rewritten.

- Was: three fixed peer tabs acting as a pane selector (agent | terminal | file). **Now:** agent
  tab + shell tab + **one tab per open file**, in open order.
- File tabs carry a close affordance: 15×15 hit box, radius 3, hover `#23282c`, `×` 11px mono —
  `#7d848b` on the active tab, `#3d4248` otherwise. Agent and shell tabs have none.
- Opening a file **adds a tab** instead of taking over the centre pane. Sources: a row in Changes,
  a file in the tree, a path in terminal output, a palette file result, `]` / `[`.
- `+` became a menu button (`+ ▾`, active bg `#171a1d`) opening a 326-wide popover 34 below it
  (`#15181b`, border `#2b3238`, radius 6, shadow 0 14 30 / .55): 29-high rows of chip · label
  (nowrap) · dim sub · hint keycaps — *New terminal* `ctrl+shift+T`, *New agent pane*
  `mod+shift+N`, *Open file…* `mod+P`, *Next changed file* `]`.
- Right end of the strip: session-jump keycaps + `session` label (was a bare `⌘ 1…8` pair).

### 5. Terminal is interactive

README §Surface B — rewritten.

- Paths and `file:line` references render as links: `#7fb4e3`, 1px **dotted** `#3d6a91` underline;
  hover `#a5cdf0` with a solid `#78a8d0` underline. Click opens the file as a tab (change 4).
- A line is authored as `[prefix, colour, link, suffix]` — the link is a span inside the line, not
  a whole-line style, so `  ↳ tests/upload.rs:88:` links only the path.
- Header: shell name is platform-dependent (`zsh` / `bash · wsl`), then the worktree path, then
  hint keycaps (split · clear · `mod` + "click a path to open it").
- **New footer band, 26 high** (`#111316`, top border `#1c2023`): `pid` · `148×38` · the
  environment chip (see change 8) · right-aligned "file:line references open in a tab".
- Failed sessions now show real panic output with two clickable frames.

### 6. Editor zoom

README §Surface C.

- Zoom group in the code toolbar, left of the 1px divider: `−` / value / `+`, each 19×19 radius 3
  hover `#1b1f22`; the value is 10px/450 mono `#8b9197` in a 36 column and **click resets to 100%**.
  Range 70–200 in steps of 10.
- Implementation note for the rebuild: code rows are authored at `1em/1.6` and the scroll
  container owns the px size (`12.5px × zoom`), so diff and file views scale together — gutters
  and diff-sign columns keep their fixed widths.
- Zoom is per editor tab (setting in change 3), independent of interface scale.

### 7. Status bar rebuilt

README §Layout, §Design tokens (band).

- Height **26 → 28**, gap 12 → 9, all values 10px mono.
- Left: branch `#8b9197` · `↑2 ↓0` · divider · **urgency counters** — five 5×5 radius-1 squares in
  the status colours with counts `#8b9197` (amber 2, red 1, green 2, blue 2, grey 1) · divider ·
  `3 agents · 41% cpu · 2.8 GB` · divider · `11 wt · 2.1 GB`.
- Right: environment chip · `5 servers · 0 errors` · divider · `ln 44, col 14` · `4 spaces` ·
  `LF` · `UTF-8` · editor zoom (clickable, resets) · `UI 100%` · divider · palette and session
  keycap pairs.
- The old single `8 sessions · 2 waiting · …` string is gone — the dot row replaces it.

### 8. Environment (WSL) chip — new

README §Settings, §Layout.

- 17-high, padding 0 6, radius 3, 9.5px mono. On Windows: `WSL · Ubuntu-24.04`, fg `#8fbde6`,
  bg `#16222c`, border `#24384a`. Elsewhere: `local · aarch64` / `local · x86_64`, fg `#6b7178`,
  transparent, border `#22262a`.
- Appears in the status bar and the terminal footer, and as the `Default environment` row in
  General. It is the one place the WSL split is visible — sessions never mix sides.

### 9. Command palette input caret

README §Command palette.

- The caret was a fixed 1.5×16 bar **after** the placeholder, which read as a UI artefact. Now it
  sits at the insertion point: **before** the placeholder when the query is empty, immediately
  after the text when something is typed. Colour unchanged (`#5a9ad4`).
- New group in the default scope: **History** — `Undo — keep all changes` (`mod+Z`) and
  `Redo — discard worktree` (`mod+shift+Z`), with the affected session as the sub-line.

### 10. Language chips extended

README §Design tokens, `tokens.rs` `lang`.

- Added `ts` `#6b9bd1` on `#1b2838`, `vue` `#5cb87f` on `#16261e`, `py` `#c9b04a` on `#2a2612`,
  `go` `#5fa8c4` on `#152730`. Same 13px/2.5-radius chip; the chip is derived from the extension
  everywhere (tree, tab, palette, LSP settings) — never hand-assigned.

### Not design changes (listed so they aren't inferred from the mockup)

Repo layout, module splitting, perf work, cross-platform build targets, WSL plumbing and LSP
integration testing carry no visual spec. Their only surfaces here are the environment chip
(8), the Language servers page (3) and the OS keymap (2).
