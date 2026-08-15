---
name: implement
description: Build a scoped GitHub issue end to end - branch, TDD loop, and handing off to ship for the gate/commit/PR. Use whenever the user says to implement or build something that has an issue number, or says "go ahead and build it", "implement the plan we just wrote", "let's write the code for #331", or picks up work after plan has already scoped an approach. Don't hand-roll branch naming manually when this skill covers it - and don't skip straight here from a bare issue number without plan first if the issue hasn't been scoped yet. For finishing work that didn't start from an issue, use ship directly instead.
---

# Implement

Turn a scoped issue into a merged-ready PR, following the conventions `CLAUDE.md` already states
so they don't have to be re-decided per change.

## Before starting

If the issue hasn't been through `plan` yet — no comment on it stating an approach — run that
first. Implementing against an unscoped issue is how a fix ends up solving the wrong problem.

## Steps

1. **Branch**: `<type>/<issue>-<slug>` (`fix/336-text-input-selection`,
   `feat/295-agent-pane-action-bar`). One convention, matching the conventional-commit prefixes
   already used in this repo's history.

2. **Write the failing test first.** This isn't a formality — `crates/app` already has real
   test-writing conventions worth matching: `#[gpui::test]` + `TestAppContext`/`VisualTestContext`
   for anything touching `Render`/`Entity`, a test module named for the concern
   (`mod change_row_selection_tests`, not `mod tests`), fixtures under a sibling `testdata/`
   directory rather than inlined strings. Look at a neighboring test module in the same file or
   feature folder before inventing a new pattern.

3. **If the fix's home isn't obvious** — which crate, whether it needs to be a Command/Query,
   whether it belongs in the domain layer or the UI — use `architecture` before writing code, not
   after. Most changes are obviously same-shape-as-a-neighboring-file and don't need this.

4. **Implement the minimum that makes the test pass**, then walk it against `rust-standards` before
   considering the step done — that's the checklist for everything a green `clippy` run doesn't
   prove (path types, git argv, `plural.rs`, no fake functionality, GPUI blocking-call offload, the
   comment rule, no new `use super::*`).

5. **Finish with `ship`** — it runs the full gate (`/check`), commits, pushes, and opens the PR
   (capturing `verify` first if the change is UI-visible, filling in
   `.github/pull_request_template.md` for real rather than leaving it generic). Link the issue this
   implementation started from; if `plan` posted an approach comment, the PR body can be short —
   the issue already carries the reasoning.

## When the plan turns out wrong mid-implementation

If the real shape of the fix diverges from what `plan` scoped — a different layer turns out to be
the right place, or the issue's actual bug isn't what the title says — stop, comment the correction
on the issue, and continue. Don't quietly implement something different from what's written down;
that's exactly the drift `plan` exists to prevent.
