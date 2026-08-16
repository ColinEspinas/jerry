# Changelog

## v0.1.0 — Search, review, and a real history view

### New features

- **Search tab** — a real right-panel Search surface: a match tree over the whole worktree, gitignore-aware and cancellable, in-place replace, and `mod+F` in-file find. The exclude pattern list is now real, user-editable settings rather than a fixed list.
- **Diff-line review notes** — leave a note on any diff line, draft it, batch several, and send them straight to the run's agent.
- **Interactive rebase, for real** — the rebase plan surface got its full design pass: real drag-to-reorder, real rebase-onto from the branch context menu, and merge actions gated with a reason shown on the row.
- **History overhaul** — a sidebar run index, per-run transcript tabs, and a real view of outcomes and drift between runs.
- **Per-agent attribution** — a diff gutter and author chips show which agent wrote which line, with a shared-file ring filter to isolate one agent's changes.
- **Provider rate-limit budget** — a real per-provider budget readout in the agent pane, showing percentage used instead of nothing.
- **Text inputs, unified** — every input surface (search, rename, commit message, review notes, terminal) now shares one row with real selection, clipboard, and mouse editing.
- **Status bar redesign** — three tiers plus a Resources popover, replacing the old flat bar.
- **Changes panel redesign** — four collapsible sections in one scroller, git status letters, and floating hover actions per row.
- Sound design for app start and agent status changes; a shared menu system powering the rail's context menus; the Phosphor icon set vendored in with a shared render helper; cross-platform per-process CPU/memory sampling on macOS and Windows.

### Improvements

- Git graph: topological walk so lines never disconnect, survives fractional display scale factors, right-click on a branch chip opens the branch menu (not the row menu).
- Terminal: real mouse-wheel/PageUp scrollback, correct back-tab (Shift+Tab) sequence, resize no longer leaves an idle pane's grid stale, empty panes stop scrolling.
- Rail: virtualized Worktrees row list (fixes slow hover with many rows), agent/history-run selection is now mutually exclusive, every row gets a real width constraint.
- One pluralisation helper (`plural::count`/`plural::form`) now backs every count shown in the window.
- Command palette can now be opened with no tab open; macOS reopens a window when the Dock icon is clicked with none open.
- A long tail of layout fixes: tab strip real-scroll and spacer cleanup, sidebar indent guides no longer pick up the accent colour, caret/blink-loop fixes, icon glyph stretching, agent-pane context bar and readout strip.

**Full Changelog**: https://github.com/ColinEspinas/jerry/compare/v0.0.3...v0.1.0
