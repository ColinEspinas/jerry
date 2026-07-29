# Restructure crates/app/src into feature/domain folders instead of a layer split

## Summary

`crates/app/src` currently organizes by *layer*: pure logic/state modules sit at the top
level (`rail.rs`, `settings.rs`, `palette.rs`, `merge.rs`, `sessions.rs`, etc.), and all
GPUI-rendering code lives under one `root/` directory (`root/rail_render.rs`,
`root/settings_render.rs`, `root/palette_render.rs`, etc.). Finding "everything about one
feature" today means checking two unrelated top-level locations.

This split was a reasonable, low-risk way to break up one originally-10,872-line `root.rs`
file during Revision R1 — but it solved "one file is too big" rather than being a
deliberately chosen long-term organizing principle. It has stopped paying for itself now
that `root/` has ~19 files (several 1000+ lines, `code_surface.rs` alone at 4747 lines) and
the corresponding logic files live in a separate, also-large ~24-file flat directory.

## Proposed design

Restructure into feature/domain folders, one per real subsystem, e.g.:

- `src/settings/{state.rs, store.rs, render.rs, widgets.rs}` (from `settings.rs`,
  `settings_store.rs`, `root/settings_render.rs`, `root/settings_widgets.rs`)
- `src/palette/{state.rs, render.rs}` (from `palette.rs`, `root/palette_render.rs`)
- `src/rail/{state.rs, render.rs}` (from `rail.rs`, `root/rail_render.rs`)
- `src/merge/{state.rs, flow.rs, render.rs}` (from `merge.rs`, `root/merge_flow.rs`,
  `root/merge_flow_render.rs`)
- `src/terminal/{pane.rs, grid.rs, links.rs}` (from `terminal_pane.rs`, `terminal_grid.rs`,
  `terminal_links.rs` — already effectively one domain, just not folder-grouped)
- `src/code_surface/` — the real subsystem behind `root/code_surface.rs` (4747 lines) and
  `root/editing.rs` (2272 lines, Revision R8.5a's real text editing). This is also the
  natural point to do the further internal split `code_surface.rs` was already flagged as
  due for (file-view rendering / diff-view rendering / zoom / editing-integration-hooks as
  separate files within this one folder, rather than one 4747-line file).
- and so on for the remaining real subsystems (sidebar/file_tree, work_surface/sessions,
  status_bar, title_bar, lsp, hover/diagnostics/completions as a language-tooling group).
  `keymap.rs`/`theme.rs`/`fonts.rs` are genuinely cross-cutting and may legitimately stay at
  the top level rather than being forced into a feature folder they don't really belong to —
  use real judgement per module, don't force every file into a folder.

The principle the current split already protects must be preserved: pure, non-GPUI-coupled
logic (plain `#[test]`-able) stays clearly separated from GPUI-rendering code (needs the
real `#[gpui::test]` harness) — within each new feature folder, not across two disconnected
top-level locations. This is a pure reorganization for navigability, not a rewrite and not a
change to the underlying separation-of-concerns discipline.

## Out of scope (separate, later, more deliberate decisions)

- Whether `AdeApp` should be decomposed from one large struct (currently ~130+ fields) into
  multiple real, separate GPUI `Entity<T>`s per subsystem — a state-ownership change,
  materially bigger and riskier than file reorganization, evaluated on its own later.
- A `Cached<K, V>` primitive to consolidate the several independently hand-rolled cache
  fields on `AdeApp` (`file_view_cache`, `diff_highlight_cache`, `merge_highlight_cache`,
  etc.) — a real, valuable, but distinct piece of work.

## Scale note

This touches nearly every file in the `app` crate. Given its size, scope this as multiple
sequential sub-phases (subsystem by subsystem) when picked up, the same way other large
revisions in this project's history (R4, R9, R8.5) were split into lettered sub-phases,
rather than one single enormous move. Best scheduled once the rest of the active roadmap has
landed and stopped churning — doing it mid-flight against other active work would either
force everything else to serialize behind it or risk real file-conflict collisions.

## Verification

Must be a genuinely behavior-preserving move, verified the same rigorous way Revision R1's
original 10,872-line `root.rs` split was: a careful token-level/structural before-and-after
comparison proving nothing was silently lost or changed in the move, not just "tests still
pass." R1's own precedent found 236 items identical, 0 missing, only benign rustfmt-wrap
diffs — that's the bar to match. Same discipline as every other revision otherwise: builder
→ independent verification (including the structural diff comparison) → adversarial checker
→ fix round → commit → BUILD-LOG entry.

## References

- BUILD-LOG.md's "Revision R1" entry (the original module split this proposal extends)
- BUILD-LOG.md's "Revision R5.5" entry (prior code-quality/organization pass)
