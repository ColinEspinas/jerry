# 0004: Retire BUILD-LOG.md and ASSESSMENT.md in favor of ADRs

## Status

Accepted.

## Context

`BUILD-LOG.md` (592 KB, 7,715 lines) was a hand-written, append-only narrative changelog of the
project's early AI-agent-driven build sessions. `ASSESSMENT.md` (24.7 KB) was a one-time,
end-of-build candid retrospective. Both were treated as living documentation: `CONTRIBUTING.md`
told every new contributor to read both before starting, and mandated updating `BUILD-LOG.md`
"alongside real functional changes."

Neither was current:

- `BUILD-LOG.md`'s last commit predates 241 of the repository's 587 commits (~41% of project
  history is undocumented in the file CONTRIBUTING called the design record).
- `ASSESSMENT.md` has exactly one commit — its own creation — and says so about itself: it opens by
  calling itself "a point-in-time snapshot from an earlier, much smaller phase of this project,"
  left as-is rather than rewritten.
- The README pointed readers to both as authoritative for current project status (`## Status`),
  while also containing a direct self-contradiction on the Wayland/X11 default that neither doc
  caught.
- Both files' style — long narrative prose, revision numbers (R1–R12, R8.5a), and design-history
  justification inline with the artifact it documents — is also the pattern
  [`../../CLAUDE.md`](../../CLAUDE.md)'s comment rule now excludes from source comments. Keeping the
  files around as the "proper place" for that material would have undermined that rule immediately.

## Decision

Delete both files. Git history retains every word of them for anyone who wants the archaeology.
Their replacement is Architecture Decision Records under `docs/adr/`, of which this file is one: one
document per decision, written once, not maintained as a running log, and explicitly *not* a
substitute for `git log`.

## Consequences

- `CONTRIBUTING.md`'s instructions to read and update `BUILD-LOG.md` are removed.
- `README.md`'s `## Status` section stops deferring to `ASSESSMENT.md` and states current status
  directly, verified against what's actually in the codebase rather than an 18-day-old essay.
- Design decisions worth recording going forward get a new ADR (`docs/adr/000N-slug.md`), not an
  entry appended to a long-running file. An ADR is written once and, if superseded, gets a new ADR
  that says so — it is not edited to stay "current."
