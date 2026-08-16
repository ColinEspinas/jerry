---
name: plan
description: Scope a GitHub issue into an implementation approach before any code gets written - pulls the issue with gh, reads the relevant code, surfaces what's actually unclear, and posts the resulting plan back as a comment. Use this whenever the user gives an issue number or URL and wants to start working on it, says things like "let's do #331", "pick up the terminal scrollback issue", "what would it take to fix #336", or wants an issue scoped/framed before implementation begins. Should run before implement, not skipped in favor of jumping straight to code.
---

# Plan

Turn a GitHub issue into an approach worth implementing, and leave a record of that approach on
the issue itself so anyone reading it later — human or another agent picking it up cold — knows
why the eventual PR looks the way it does.

## Why this is its own step

Jerry's issues (`gh issue list`) are frequently underspecified on purpose — a one-line bug report,
a screenshot, a design note. Writing code against that directly means guessing at the actual
requirement mid-implementation, which is how scope drifts and how a fix ships that solves the
symptom in the issue title but not the underlying problem. Scoping first, in writing, catches that
before any code exists to be attached to the wrong fix.

## Steps

1. **Pull the issue.** `gh issue view <n> --json title,body,labels,comments`. Read every comment,
   not just the body — a real discussion often narrows or overturns the original ask.

2. **Read the actual code the issue touches**, not just enough to guess. If the issue names a
   symptom ("terminal scrollback doesn't work"), find the module responsible
   (`crates/app/src/terminal/`) and read enough of it to know whether the fix is local or touches
   a boundary this project cares about — see `CLAUDE.md`'s architecture section: does this cross
   from render code into an adapter? Does it touch a core crate that must stay `gpui`-free?

3. **Surface what's actually unclear**, not everything that could theoretically be asked. Most
   issues have zero or one real open question. If there's a genuine fork in approach (e.g., fix at
   the terminal-grid layer vs. the PTY layer), that's worth a question; "should I write tests" is
   not — CLAUDE.md already answers that.

4. **Check for a design source before inventing a UI shape.** If the issue touches layout, colors,
   or interaction states, `design_handoff_jerry_ade/revision/` is the authoritative reference —
   check it before proposing something that just looks reasonable.

5. **Write the approach**, scaled to the issue: a paragraph for a small bug, a short structured
   plan (what changes, which files, what the test coverage looks like) for anything touching more
   than one file. State it plainly rather than hedging — if there are two reasonable approaches,
   say which one you'd take and why, not both wrapped in "it depends."

6. **Post it back to the issue**: `gh issue comment <n> --body "..."`. This is what makes the
   scoping durable — the next person (or the next session) reads the issue and sees the plan, not
   just the bug report.

7. **Hand off to `implement`** with the issue number. Don't start writing code in this skill —
   scoping and building are deliberately separate so a bad plan gets caught before it's expensive
   to unwind.

## What "done" looks like

A comment on the issue that states the approach, names the files it touches, and flags anything
genuinely uncertain — not a restatement of the issue, and not a full implementation plan document
(that's overkill for most of these; save the heavier planning process for something that's
actually architectural).
