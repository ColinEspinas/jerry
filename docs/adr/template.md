# 000N: <short, decision-stating title>

<!-- Title states the decision, not the topic: "Command/Query as the application-layer unit",
     not "Application layer design". Someone scanning docs/adr/README.md should be able to tell
     what was decided from the title alone. -->

## Status

<!-- One of: Accepted / Superseded by [000M](./000M-slug.md) / Deprecated (with why). An ADR is
     never edited back to "current" after the fact - if a later decision changes it, that later
     decision is its own new ADR, and this one's status is updated to point at it. -->

Accepted.

## Context

<!-- What prompted this - the concrete problem, not a generic motivation. Cite real evidence:
     file paths, line counts, an actual measured number, a real failure mode. If you considered
     and rejected an alternative, say what it was and why it lost - that's what stops the same
     debate from repeating later. -->

## Decision

<!-- What was decided, stated plainly. If it's a rule ("no crate other than X may depend on Y"),
     say so as a rule, not a suggestion - and say what enforces it (a lint, a hook, or "nothing
     yet, tracked as an issue") rather than leaving enforcement implicit. -->

## Consequences

<!-- What this changes going forward, and what it deliberately does NOT retroactively fix if
     the codebase doesn't already match it - be explicit about the gap rather than implying
     the decision is already fully realized. If something is enforced mechanically (a lint, a
     ratchet check), name the exact mechanism and where it lives. -->
