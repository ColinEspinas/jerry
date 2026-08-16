---
name: ship
description: Finish a change and open a real PR - run the gate, capture a screenshot for anything UI-visible, commit, push, and fill out .github/pull_request_template.md with a real summary instead of a generic one. Use whenever the user says to ship it, open a PR, wrap up the change, finish this task, or "commit and push this" - whether or not the work went through plan/implement first. implement's own last step calls this rather than duplicating it, so use ship directly for anything that didn't start from a scoped GitHub issue.
---

# Ship

Turn a working tree into a PR someone can actually review without having to reconstruct what
changed and why from the diff alone.

## Steps

1. **Run the gate.** `/check` (conventions + fmt + clippy `-D warnings`). Don't proceed past a
   failure — a PR opened against a red gate just moves the failure to someone else's screen.
   `cargo test --workspace` isn't part of this gate right now
   ([issue #348](https://github.com/ColinEspinas/jerry/issues/348)) — run whatever tests are
   relevant to the change manually (e.g. `cargo test -p wt-core`, or a scoped `cargo test -p app
   --lib <module>::`) and note the result in the PR's Testing section instead of leaving it blank.

2. **Decide if this is UI-visible.** `git diff --name-only` against the target branch — did it
   touch a `render.rs`, `theme.rs`, or anything else that changes what the app looks like? If so,
   run `verify` and keep its final capture; if not, skip straight to committing rather than forcing
   a screenshot onto a logic-only change.

3. **Commit.** Conventional style (`feat(app): ...`, `fix(pty-core): ...`), matching this repo's
   existing history. If there are several logically separate pieces of work in the tree, several
   focused commits beat one large one — but don't split an atomic change just to inflate the
   commit count either.

4. **Push** the current branch (`git push -u origin HEAD` on a first push).

5. **Open the PR** with `gh pr create --body-file`, filling in
   `.github/pull_request_template.md`'s sections for real rather than leaving its placeholders:
   - `<!-- Closes #<issue> -->` — the actual issue number if there is one; delete the line entirely
     if this change has no tracked issue.
   - **What** — what changed, specifically, not a restatement of the commit list. If `plan` already
     posted an approach on the issue, this can be short.
   - **Why** — only if it isn't obvious from the issue/title; delete the section otherwise.
   - **Testing** — check off what was actually run (`/check`, `verify` if step 2 triggered it, the
     scoped tests from step 1); note anything that couldn't run and why, rather than leaving an
     unchecked box unexplained.
   - **Architecture notes** — only if this touches the crate boundary or added/changed a
     Command/Query (e.g. "new Command: `AttemptCherryPick` in `wt-core`"); delete the section for a
     change that doesn't touch any of that.
   - **Screenshot** — the `verify` capture, if there is one; delete the section entirely for a
     logic-only change rather than leaving it empty.

## What "a real summary" means

Not "various fixes and improvements," and not a bullet list that just repeats file names. State
what the change actually does from a reviewer's point of view — what breaks or works differently
now that didn't before. If the change is small, the summary should be small too; padding a
one-line fix into three paragraphs is its own kind of dishonesty about scope.

## If the gate or verify can't run

If `/check` can't complete (e.g. a genuinely broken toolchain, not just a failing test) or `verify`
can't get the app's permissions to capture, say so plainly in the PR body rather than silently
skipping the section — "not run: <reason>" is honest, a missing section that looks like an
oversight isn't.
