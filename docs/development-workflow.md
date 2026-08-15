# Development workflow

How a change moves through this repo, end to end — which skill/command covers which step, and
where to look when something doesn't fit the happy path. `CLAUDE.md` has the standards; this is the
process that applies them.

```
 GitHub issue                                                          merged
      │                                                                   ▲
      ▼                                                                   │
   ┌───────┐   unclear home?   ┌──────────────┐                    ┌──────────┐
   │ plan  │ ─────────────────▶│ architecture │                    │  review  │
   └───┬───┘                   └──────┬───────┘                    └────▲─────┘
       │  scoped approach              │  crate/layer/Command decision  │
       │  posted to the issue          ▼                                │ PR opened
       └──────────────────────▶ ┌──────────────┐   standards check ┌────┴────┐
                                 │  implement   │ ──────────────────▶│  ship   │
                                 │ (branch, TDD)│  rust-standards     └─────────┘
                                 └──────────────┘  verify (if UI)         ▲
                                                                          │
                                          ad hoc change, no issue ────────┘
```

## The steps

1. **Pick up work.** From a labeled, open issue (`gh issue list`, or `triage` first if the tracker
   needs a sweep) — or ad hoc, for something too small to warrant one.

2. **Scope it — `plan`.** Reads the issue and its comments, reads the actual code it touches,
   checks `design_handoff_jerry_ade/revision/` if it's UI-shaped, and posts the approach back to
   the issue as a comment. Skip this only for genuinely trivial changes; most drift in this project
   comes from implementing against an unscoped one-line bug report.

3. **Decide where it belongs, if that's not obvious — `architecture`.** Which crate, whether it
   needs to be a Command/Query, whether it's UI-only glue. Most changes are obviously
   same-shape-as-a-neighboring-file and skip straight past this step.

4. **Build it — `implement`.** Branches (`<type>/<issue>-<slug>`), writes the failing test first,
   implements against the standards, then hands off to `ship`. This is the skill that owns the
   TDD loop; it doesn't duplicate the gate or PR steps itself.

5. **Hold the line on standards as you go — `rust-standards`.** The checklist for everything
   `cargo clippy -D warnings` doesn't catch mechanically: path types, git argv, the pluralization
   helper, no fake functionality, GPUI blocking-call offload, the comment rule, no new glob
   imports. Run it before calling any step done, not only at review time.

6. **See it, if it's visible — `verify`.** Launches the app, screenshots it, checks the result
   against `design_handoff_jerry_ade/revision/` or the issue's acceptance criteria. The only way
   UI-visible work gets checked against something real instead of "looks right."

7. **Finish — `ship`.** Runs `/check` (the full gate: fmt, clippy, tests), commits, pushes, and
   opens the PR from `.github/pull_request_template.md` with a real summary — not a generic one.
   Works standalone for anything that didn't start from a scoped issue, too.

8. **Get reviewed — `review`.** Checks a diff or PR against `CLAUDE.md`'s standards and posts real
   findings with `gh pr review`, not a line-by-line narration.

9. **Keep the tracker usable — `triage`.** A periodic batch pass, not part of any single change's
   flow: labels, catches duplicates, flags stale issues.

## Commands vs. skills

`/check` and `/setup` are commands — deterministic, cheap, no judgment involved (`/check` just runs
three cargo invocations in order). Everything above is a skill because it requires judgment: what
counts as "UI-visible," what a real (non-generic) PR summary looks like, whether something needs a
new crate. Reach for the command when the answer is fixed; reach for the skill when it depends on
the change.

## Agents

`builder`, `checker`, and `finder` (`.claude/agents/`) are for delegated or parallel work inside a
single step, not the overall flow above: `finder` verifies a GPUI/`alacritty_terminal`/`gix`
signature before it's used, `checker` does a read-only audit of a diff, `builder` implements one
assigned step end to end when work is being split across subagents.

## Where the pieces live

| What | Where |
|---|---|
| Standards (what "correct" means) | [`CLAUDE.md`](../CLAUDE.md) |
| Target architecture | [`docs/architecture/`](./architecture/), [`docs/adr/`](./adr/) |
| Skills | `.claude/skills/*/SKILL.md` |
| Commands | `.claude/commands/*.md` |
| Agents | `.claude/agents/*.md` |
| Issue/PR templates | `.github/ISSUE_TEMPLATE/`, `.github/pull_request_template.md` |
| Human contribution process | [`CONTRIBUTING.md`](../CONTRIBUTING.md) |
