---
name: triage
description: Sweep open GitHub issues to apply labels, catch duplicates, and flag issues that are actually blocked or out of scope - keeps the tracker usable as it grows past a few dozen open issues. Use when the user asks to triage issues, clean up the issue tracker, label unlabeled issues, says "go through the backlog", "sort the open issues", or wants a batch pass over `gh issue list` rather than working a single issue (that's plan/implement instead).
---

# Triage

A batch pass over the issue tracker, not a single-issue workflow — this skill exists because most
issues here get filed and never labeled, which makes `gh issue list` progressively less useful as a
"what should I work on" view.

## Label taxonomy

- **Type** (GitHub defaults): `bug`, `enhancement`, `documentation`, `question`.
- **Area**: `area:core` (wt-core/pty-core/lsp-core, or the target architecture in
  `docs/architecture/`), `area:editor` (code surface, LSP UI), `area:terminal`, `area:ui` (rail,
  sidebar, settings, theme), `area:ci` (workflow, tooling).
- **Status** (project-specific, already in use): `backlog` (deliberately deferred, revisit later),
  `blocked-on-design` (needs a design pass before it can be specced).

Don't invent new labels ad hoc — if an issue doesn't fit any existing one, that's worth flagging to
the user rather than silently adding a new label to the taxonomy.

## Steps

1. **Pull the current state**: `gh issue list --state open --limit 100 --json
   number,title,labels,body,createdAt`.

2. **For each unlabeled issue**, read enough of the body to assign type + area confidently. If the
   type or area genuinely isn't clear from the title and body alone, don't guess — leave it and
   note it as ambiguous in the summary rather than mislabeling.

3. **Check for duplicates** against the rest of the open list — same symptom, same component,
   filed close in time. A real duplicate gets `gh issue comment <n> --body "Duplicate of #<other>"`
   plus `gh issue close <n> --reason "not planned"`, not just a label.

4. **Flag stale-looking issues**: something that reads as already fixed by later work, or that
   references a file/mechanism that no longer exists (check before assuming — a search that comes
   back empty is real evidence, a guess isn't). Don't close these unilaterally; comment asking for
   confirmation, or list them in the summary for the user to decide.

5. **Apply labels** with `gh issue edit <n> --add-label "..."`.

## Output

A short summary, not a blow-by-blow: how many issues were labeled, how many duplicates were found
and closed, and a list of anything ambiguous that needs a human call — not an exhaustive log of
every `gh` command run.
