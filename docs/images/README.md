# README images

Every file in this directory is currently a **generated placeholder**, not a real capture — a flat
three-zone wireframe in Jerry Dark's palette, captioned "screenshot pending". They exist so the
product README renders cleanly instead of showing broken image slots while the real assets are
being produced.

Replacing them is tracked in [issue #431](https://github.com/ColinEspinas/jerry/issues/431).

## Shot list

Each capture replaces the same filename, at roughly the same aspect ratio, with the app running
against a real repo and real agents — no mocked rows, no empty states standing in for populated
ones.

`hero.png` is the only full-window shot. It renders at 960px, so the whole three-zone layout has room
to read. **Every other image renders in a 50%-wide table cell**, roughly 400px on a desktop browser —
a full-window screenshot shrunk to that is illegible. Crop each one to its own surface, close enough
that the thing the block is about is the thing you see.

| File | Surface | What has to be visible |
| --- | --- | --- |
| `hero.png` | Whole window (960px) | All three zones at once: the rail with several sessions in different states, an agent mid-run in the work surface, its diff on the right. |
| `rail.png` | Session rail | Several worktrees across at least two repos, with agent rows showing different derived statuses and elapsed times. |
| `terminal.png` | Work surface | An agent CLI's own live TUI rendering correctly, with the per-worktree tab strip and a shell tab beside it. |
| `review.png` | Diff + review notes | A real diff against a detected base branch with at least one line-anchored note attached. |
| `editor.png` | Code editor | Syntax highlighting plus a visible LSP affordance — a diagnostic, hover, or completion popup. |
| `conflicts.png` | Merge surface | A genuinely conflicted merge with the per-hunk accept/reject controls showing. |
| `graph.png` | Git graph | A branch topology worth looking at — several branches and worktree markers, not a single line. |

## Producing them

`/verify` drives the app and captures the window. Prefer a real repo with real history; scrub
anything in frame that shouldn't be public (absolute paths under a home directory, private branch
names, agent conversation content).

Keep them reasonably sized — these all load on the repo's front page, unlazily.

PNG for static surfaces. A GIF is worth it only where motion carries something a still can't (an
agent's status flipping in the rail, a TUI redrawing, notes landing in a PTY). Where one is used,
pair it with a still fallback rather than shipping the GIF alone:

```html
<picture>
  <source srcset="docs/images/rail.gif" type="image/gif">
  <img src="docs/images/rail.png" alt="..." width="100%" />
</picture>
```
