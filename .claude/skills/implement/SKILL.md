---
name: implement
description: Build a scoped GitHub issue end to end - branch, TDD loop, the pre-commit gate, and a PR that links the issue. Use whenever the user says to implement, build, fix, or ship something that has an issue number, or says "go ahead and build it", "implement the plan we just wrote", "let's write the code for #331", or picks up work after plan has already scoped an approach. Don't hand-roll branch naming or the gate manually when this skill covers it - and don't skip straight here from a bare issue number without plan first if the issue hasn't been scoped yet.
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

3. **Implement the minimum that makes the test pass**, respecting the architecture boundary in
   `CLAUDE.md`: render code dispatches, it doesn't call `wt_core::`/`pty_core::`/`lsp_core::` or
   shell out directly (that's the target — existing violations in the file you're touching aren't
   yours to fix unless the issue is about them, but don't add new ones). No `unwrap()`/`expect()`
   outside test code, no `unsafe` without a justified `SAFETY` comment and a local
   `#[allow(unsafe_code)]`, `PathBuf` not `String` for paths, git as explicit argv never a shell
   string, counts through `crates/app/src/root/plural.rs`.

4. **No fake functionality.** If a piece genuinely can't be built in this pass, say so visibly (a
   real "not implemented" state, or `todo!("unverified: ...")` with a real explanation) — never a
   plausible-looking stand-in bound to nothing.

5. **Comments**: only a non-obvious *why*, per `CLAUDE.md`. Don't narrate design history or
   alternatives-considered in the source — that belongs in the commit body or, for something
   genuinely architectural, a new `docs/adr/000N-*.md`.

6. **Run the gate before every commit**: `/check` (fmt + clippy `-D warnings` + the full test
   suite). The `.claude/hooks/pre-commit-check.sh` hook only catches fmt/clippy automatically —
   it does not run tests, so don't treat a clean commit as proof the suite passes.

7. **Commit**, conventional style (`feat(app): ...`, `fix(pty-core): ...`), focused — the shape
   the existing `git log` already models.

8. **Open the PR** with `gh pr create`, body linking the issue (`Closes #<n>`). If `plan` posted an
   approach comment, the PR description can be short — the issue already carries the reasoning.

9. **If the change is UI-visible**, run `verify` before opening the PR — attach the capture it
   produces to the PR description rather than describing the result in prose.

## When the plan turns out wrong mid-implementation

If the real shape of the fix diverges from what `plan` scoped — a different layer turns out to be
the right place, or the issue's actual bug isn't what the title says — stop, comment the correction
on the issue, and continue. Don't quietly implement something different from what's written down;
that's exactly the drift `plan` exists to prevent.
