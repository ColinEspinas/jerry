---
name: review
description: Review a diff or an open PR against CLAUDE.md's standards and post findings with gh pr review, rather than just describing problems in chat. Use whenever the user asks to review code, review a PR, check a diff before merging, says "review #340", "can you look over this PR", "review my changes before I push", or wants a second pass on someone else's or their own work. Distinct from the checker agent (which audits a single step mid-build for fake functionality) - this is for a complete diff or PR, and it posts to GitHub rather than just reporting inline.
---

# Review

Read a diff the way a careful reviewer on this project would, then post what actually matters —
not a line-by-line narration of the change.

## Scope the review first

Figure out what's actually being reviewed before reading code:

- **A PR number**: `gh pr view <n>`, `gh pr diff <n>`.
- **The working tree**: `git diff` against the target branch (usually `main`).
- **A branch or commit range**: `git diff <base>...<head>`.

## What to check, in priority order

1. **Correctness.** Does it do what it claims? For anything touching git operations, process
   spawning, or the terminal/PTY layer, trace the actual failure modes — a merge that doesn't
   handle a conflict state, a process that isn't reaped, a path built by string interpolation
   instead of argv.

2. **No fake functionality.** This project's own hard rule: no UI bound to hardcoded data standing
   in for something real, no simulated output, no control that looks wired up but isn't dispatching
   anything. This is the single most-flagged issue in this codebase's own history — check for it
   specifically, not just incidentally.

3. **The standards in `CLAUDE.md`.** `unwrap()`/`expect()` outside tests, unjustified `unsafe`,
   `String` where a path should be `PathBuf`, an interpolated git command, a hardcoded plural, a new
   `use super::*` glob, render code calling an adapter directly instead of dispatching.

4. **Comments.** Flag comments that restate the code, or that narrate design history/alternatives
   that belong in the commit body or an ADR instead. Don't flag files the diff doesn't touch — this
   project isn't doing a mass cleanup, just holding the line on new code.

5. **Test coverage.** Is the new behavior actually exercised, with a real assertion, not just
   compiled? For GPUI-touching code, is it `#[gpui::test]`-based rather than a stdout snapshot that
   can't see a retained-mode UI?

## What not to do

Don't produce a line-by-line walkthrough of unchanged logic, and don't flag style preferences that
aren't in `CLAUDE.md` — this project explicitly favors real findings over volume. If nothing is
wrong, say so plainly rather than manufacturing a nitpick to have something to report.

## Posting the review

For a PR, post real findings with `gh pr review <n> --comment --body "..."` (or `--request-changes`
for something that shouldn't merge as-is, `--approve` when it's clean). Reference file:line for
each finding so it's actionable without re-reading the whole diff. For a working-tree review with
no PR yet, report findings in chat instead — there's nothing to post to.
