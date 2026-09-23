---
name: jerry
description: Use the jerry CLI to check where you are, create a sibling git worktree (optionally with another agent running in it), see which agents Jerry is supervising, and merge your branch back - whenever the task involves splitting work across worktrees, spawning a helper agent, or landing a finished branch through Jerry rather than raw git.
---

# jerry

`jerry` talks to the Jerry instance supervising this repository, if one is running. It exists so
an agent never has to guess at Jerry's internal state or reach for raw `git worktree`/merge
commands when Jerry already has a safer, typed path for the same thing.

Every subcommand accepts `--json` for machine-readable output (a `Report` on stdout: `{"status":
"ok"|"denied"|"error", ...}`) and prints human-readable text otherwise. Diagnostics always go to
stderr, never stdout.

## Exit codes

Every `jerry` invocation exits with one of these. Codes are only distinct when you must react
differently:

| Code | Meaning | What to do |
|------|---------|------------|
| 0 | Done; nothing further needed | Continue |
| 1 | Succeeded, but you have work to do next (e.g. a merge left conflicts) | Read stdout for what's left |
| 2 | Usage mistake | Fix the invocation |
| 3 | Refused; nothing happened | Do not retry the same call; escalate to the human if you're stuck |
| 4 | No Jerry reachable when one was required | Fall back to raw git, or tell the human |
| 5 | Execution failed; state may be intermediate | Check `jerry status` before retrying |

## Commands

### `jerry status`

Prints the worktree, repository, caller identity (you, as an agent, or a human), and whether a
Jerry is reachable. Cheap; safe to run any time you're unsure where you are.

### `jerry wt new <branch> [--from <ref>] [--agent <kind>] [prompt]`

Creates a new git worktree on a fresh branch, as a sibling of the main checkout. Prints the new
worktree's path on success.

- `--from <ref>`: the start point for `<branch>` (defaults to `HEAD`).
- `--agent <kind>`: also asks Jerry to start an agent CLI in the new worktree once it's created.
  `<kind>` is one of `claude`, `codex`, `cursor` (case-insensitive).
- `[prompt]`: an optional initial message handed to the spawned agent. Only meaningful with
  `--agent`.

Use this when a task genuinely splits into independent pieces of work that should run in
parallel, each on its own branch - not for a plain `git worktree add`, which still works but
gives Jerry nothing to supervise.

Creating the worktree itself works even with no Jerry running (it's a real `git worktree add`
under the hood). Spawning `--agent` requires a running Jerry: without one, the worktree is still
created, but exits 4 with a note on stderr that no agent was spawned.

### `jerry agents [--json]`

Lists every agent Jerry is currently supervising, one per line as `<id>\t<kind>\t<worktree>`
(or a JSON array with `--json`). Requires a running Jerry (exit 4 without one). Empty output with
exit 0 means Jerry is running but supervising nothing right now.

### `jerry merge [--dry-run | --continue | --abort]`

Merges this worktree's branch into the repository's base branch, through Jerry's own merge flow
rather than a bare `git merge`:

- No flags: attempts the merge. A clean merge is committed immediately (exit 0). A conflicted
  merge leaves conflict markers on disk and exits 1, listing the conflicted files.
- `--dry-run`: reports whether the merge could run right now, without running it.
- `--continue`: after you've resolved every conflict marker by hand, stages the resolved files
  and completes the merge. Still-unresolved files are listed again with exit 1.
- `--abort`: gives up on the in-progress merge and restores the base worktree.

### `jerry skill`

Prints this document.

## When to reach for `jerry` instead of raw `git`

- Splitting a task across parallel branches: `jerry wt new`, not `git worktree add` by hand -
  Jerry can then also start a helper agent there for you.
- Landing a finished branch: `jerry merge`, not `git merge` - Jerry's flow is what the human
  reviewing your work also sees and drives from the GUI, so the two stay in sync.
- Anything else (reading history, diffing, staging unrelated to a merge) is still plain `git`;
  `jerry` only covers what needs Jerry's own state or supervision.
