# Zone 3 — files, search and changes

- **Code:** `crates/app/src/sidebar/`, `crates/app/src/search/`
- **Tokens:** `theme::{tree, changes, lang, band, zone}`

## What it's for

Zone 3 is for **navigating** the focused worktree — finding the thing you want to look at. It never
shows the thing itself. Clicking a file in the tree, a search hit, or a row in Changes opens it as a
tab in Zone 2.

That division is the whole design: *centre tabs hold things you read or work in; the sidebar holds
things you navigate*. It is also why the panel has no diff view of its own, however tempting a
preview would be.

## Structure

Header (`theme::band::CHROME_HEADER`, sharing its y with the other two column headers — see
[`layout.md`](./layout.md#one-header-rule-across-three-columns)) carrying a segmented **Files ·
Search · Changes**. Files is first and the default; the review flow opens straight into Changes.
Each tab's icon is drawn in one shared optical box (`icons::IconSize::PanelTab`) — a first cut that
sized each to suit itself put three different weights on one row.

### Files

A flattened, indented row list from a real `std::fs::read_dir` walk (`sidebar::file_tree`), rendered
virtualised. Rows carry the file's language chip (see [`vocabulary.md`](./vocabulary.md#chips)) and
a mark for changed files. Folder expansion is real, per-worktree state persisted to disk
(`sidebar::fold_state`), so reopening a worktree restores the shape of the tree you left.

Live refresh is a `notify`-backed OS watcher that sets a flag, polled by a GPUI background loop
(`sidebar::file_tree_watch`) — the same split `rail::worktree_watch` uses, and the reason neither
one needs a `gpui` dependency.

Right-clicking gives the file operations menu (`sidebar::context_menu` for the pure row model,
`sidebar::file_ops` for the primitives, `sidebar::tree_ops` for the sequencing). The popover itself
is the app's one shared menu component (`menu/`) — because right-click actions on a row and an `⋯`
overflow are both "a list of actions", and two idioms for one thing drift.

Delete goes to the real OS trash where one exists; name validation and collision-free renaming are
pure functions in `file_ops`, tested without touching a filesystem.

### Search

A real panel, not a filename filter. The verdict it was built to satisfy: **a search result that
only names a file is an index, not a result — show the line.**

Structure, top to bottom: query row (sharing `theme::band::FILTER_ROW` with the rail's own filter —
the same object in two places), a revealed replace row, revealed `include`/`exclude` glob rows, a
count row with the modifier buttons, then a **two-level result tree** — file rows, each expanding to
match rows showing the line with its hit highlighted.

The pure halves are worth knowing about because they carry the semantics: `search::glob` is the
pattern language, `search::exclude` is the always-on `target`/`node_modules`/`.git` exclusion
applied *before* `.gitignore` is consulted at all, and `search::engine` is the compiled matcher, the
bounded walk, and the real on-disk replace. `search::in_file` is the `mod+F` find bar over the open
buffer, reusing the same three-state count.

### Changes

Four stacked sections (`sidebar::sections::ChangesSection`), in a fixed render order that
`ChangesSection::ORDER` is the single place recording:

| Section | Answers |
|---|---|
| `UNCOMMITTED` | what is dirty in this worktree right now |
| `COMMITS` | what has been committed on this branch |
| `AGAINST <BASE>` | how this worktree differs from the merge-base with its base branch |
| `RUNS` | the same changes, indexed by which agent produced them |

The first three are **one ladder of git state**, narrowing to widening. Runs is not on that ladder —
it re-indexes the same changes by author — so it sits after the ladder rather than inside it, which
also keeps `UNCOMMITTED`'s top edge fixed however many agents have run.

Rows carry a **seen** mark, a directory, a name, a tag pill, `+n −n`, and a stat bar.

`sections::SeenFiles` is deliberately **not** the staged set. A file counts as seen only if it was
marked *and still has the diffstat it had when marked* — so a file you reviewed and an agent then
touched again reverts to unseen on its own. Marks are keyed by worktree then by repo-relative path,
so switching worktrees and back does not lose your progress, and one worktree's counter never leaks
into another's.

## Rules that matter

- **The panel navigates; the centre displays.** No diff, no file preview, no transcript rendered
  here. A row click opens a Zone 2 tab.
- **Files is the default tab.** Changes is where a review flow lands you, not where you start.
- **`ChangesSection::ORDER` is the only place the section order is written down.** Re-deriving it at
  a render site is how the two get out of sync.
- **Seen ≠ staged.** Two different questions; conflating them would make "reviewed" survive an
  agent's next edit.
- **Panel-tab icons share one optical box.** Never one size per icon.
- **The Zone 3 header shares a y with two other headers.** Changing its height is a three-column
  edit.
- **Exclusion happens before `.gitignore`.** The always-on list in `search::exclude` is not
  negotiable by repository config; the gitignore layer is a separate, toggleable one on top.

## Not built yet

- **Search has no cross-worktree scope.** It walks the focused worktree. Searching every open
  worktree at once is not wired.
- **Changes is per worktree, not per repo.** The Runs section indexes the focused worktree's
  provenance union only.
