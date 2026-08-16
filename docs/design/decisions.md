# Design decisions

Why Jerry's UI looks and behaves the way it does — the reasoning behind a call, not just the rule.
[`principles.md`](./principles.md) and the surface pages state each rule in a line or two and point
here for the argument, deliberately, so they stay short.

This is a **decisions log, not a running narrative**, and it mirrors
[`docs/architecture/decisions.md`](../architecture/decisions.md)'s format for exactly the reason
that file gives: a growing chronicle nobody can tell is current is worse than no record. A decision
here is written once. If a later decision changes an earlier one, it gets its **own new numbered
entry** at the bottom and the old entry's `Status` line is updated to point at it — never edited
back to "current".

Add an entry for a real decision: a new rule, a reversal, or a call a future contributor would
otherwise re-litigate. Not for every routine application of one already recorded.

Entries 1–6 are seeded from calls made during the original design work and recorded until now only
in handoff prose or in a code comment. They are stated here so they can be argued with.

## 1. The rail answers "who needs me" pre-verbally

**Status:** Accepted.

**Context:** Supervising six-plus agents at once is a triage problem before it is anything else. A
list of agents with status labels is readable, but reading it is a serial operation: eyes down the
column, one row at a time, in a UI you are checking every few minutes all day.

**Decision:** The rail is ranked by urgency at every level (`WorktreeRow::urgency_rank`,
`Status::urgency_rank`) and each row carries a coloured left edge in its status colour. The intended
interaction is *seeing how tall the amber block at the top is*, not reading labels.

**Consequences:** Colour becomes a scarce resource that has to be protected (entry 2). Any change
making the answer require reading is a regression however much information it adds — sorting
alphabetically, colouring rows by agent instead of by status, or replacing the edge with an icon all
break it. This is [`principles.md`](./principles.md) rule 1.

## 2. Colour is reserved for status and diffs

**Status:** Accepted.

**Context:** Entry 1 only works if amber means exactly one thing everywhere in the product. Every
decorative accent spends a little of that.

**Decision:** The status palette (`theme::status`), the diff palette (`theme::diff`), agent tints
(`theme::agent`) and syntax highlighting (`theme::syntax`) are the entire colour budget. Chrome is
grey.

**Consequences:** New surfaces get grey chrome and borrow an existing semantic colour where they
need one. Agent tints, being the one non-status use of saturated colour, are constrained by their
own allocation rule — see entry 9. This is [`principles.md`](./principles.md) rule 2.

## 3. There is no chat UI

**Status:** Accepted.

**Context:** The obvious shape for an app that supervises agent CLIs is a chat client: parse the
agent's output, render its questions as cards, its answers as bubbles. Every other product in the
category does some version of this.

**Decision:** The agent runs in a real pty and Jerry renders its output **verbatim**, cell by cell
(`terminal::grid` over `alacritty_terminal`). The agent's question is *its own* numbered prompt, not
a card Jerry designed. The pane header shows the real invocation, the real pid and the real pty
state.

**Consequences:** Jerry works with any agent CLI on day one, including ones that don't exist yet, and
never lies about what the agent said. Structural information about agent state has to come from real
side channels — the terminal title (`rail::title_signal`), OSC sequences (`terminal::osc`), and
Claude Code's own hook system (`hooks/`) — rather than from parsing rendered output. Anything Jerry
adds around the agent goes in the chrome above and below the pane, never inside it. It also means
Jerry cannot offer features that require understanding the conversation, and that is accepted.

## 4. Every icon is composed from rects and text glyphs

**Status:** **Superseded by entry 8.**

**Context:** The original design had no image assets at all. Every icon was absolutely-positioned 1px
`div`s and Unicode glyphs, so nothing needed an SVG pipeline and the whole UI ported cheaply to any
toolkit.

**Decision:** No icon assets. Compose from rects and glyphs.

**Consequences:** Held for the whole first build. What it cost is recorded in entry 8.

## 5. `Accept file` is always rendered, dimmed

**Status:** Accepted.

**Context:** The code surface's toolbar carries a `Diff | File` segmented toggle and, next to it, an
`Accept file` button. The natural implementation hides the button when there is nothing to accept.

**Decision:** It is always rendered, dimmed and non-interactive when unavailable.

**Consequences:** Layout under the cursor never moves. Hiding it reflows the toggle sitting beside
it — under the pointer that was about to click the toggle. Generalised into
[`principles.md`](./principles.md) rule 6: controls dim, they do not vanish. `FooterAction::implemented`
applies the same treatment to the agent pane's actions.

## 6. Merge-conflict columns are headed by their agent

**Status:** Accepted.

**Context:** Git presents a conflict as "ours" and "theirs" — a framing anchored on which side ran
the merge.

**Decision:** Each column in the conflict surface is headed by **the agent whose work it is**: agent
badge, agent name, branch, commit count, and its own lines tinted with that agent's colour. Never
"ours"/"theirs".

**Consequences:** In Jerry the two sides of a conflict are two agents you know by name and by tint,
and the surface reads as *these two agents disagree here* rather than as a git operation. The same
reasoning makes Jerry propose the resolution where it can — a pre-flight strip states how many files
auto-resolve because their edits don't overlap, and `Take both` is the primary action when both edits
can be kept.

## 7. The rail has one structure: repo → worktree → agent

**Status:** Accepted. Supersedes the original `by urgency / by project` grouping toggle.

**Context:** The original rail had two grouping modes behind a header control, plus a sort control.
"By urgency" grouped agents under status headers; "by project" grouped them under repositories, and
was the only mode that could show worktrees with no agent in them at all.

**Decision:** One structure, always: **repo group → worktree → agents.** The mode toggle, the sort
control and their state were deleted, not hidden. Urgency became *ranking within* the fixed structure
— worktrees ordered by their most urgent agent, repos by their most urgent worktree.

**Consequences:** Entry 1's property is preserved (the most urgent things still float to the top and
wear a colour) while the rail keeps one shape a user can build spatial memory of. Worktrees with no
agent are always visible, which was the only real argument for the second mode. Repo headers carry
**two** urgency counts, red and amber, rather than one merged amber count: merging them said "three
worktrees want you" when one of the three had actually died. That generalises to a rule stated in
[`layout.md`](./layout.md#rules-that-matter) — two states distinguished anywhere in the app are never
summed anywhere in it.

## 8. Shipped icons are Phosphor SVGs; Jerry's own vocabulary stays hand-drawn

**Status:** Accepted. Supersedes entry 4. (GitHub issue #282.)

**Context:** Entry 4's rule held through the first build and produced, in the words of the review
that ended it: "mismatched optical sizes in a row, two glyphs from one family a divider apart, marks
that read as nothing at 17px". Hand-composed icons have no shared canvas, so two of them side by side
have no reason to look like siblings.

**Decision:** Vendor real [Phosphor](https://phosphoricons.com) SVGs (MIT) under `assets/icons/` for
**actions and views only** — panel tabs, sidebar-strip cells, the overflow menu, the terminal tab,
the prune button. `icons::Icon` is that closed list. Everything that is Jerry's own semantic
vocabulary — agent tint chips, status dots, file-extension chips, diff gutter marks, graph lanes and
merge elbows — **stays hand-drawn**, because a third-party icon family has no opinion about what
those mean.

The deciding argument for Phosphor specifically was the build target: it ships raw SVGs and GPUI
renders SVG natively, so each icon is a named asset rather than geometry someone has to reinterpret
as paths.

**Consequences:** Two mechanical rules came with it, both enforced by tests rather than convention:
icons draw only through an `IconRow` so a row shares one optical box, and files are vendored at
`bold` weight because `regular`'s stroke reads thin below 20px. Every vendored file is on the same
`0 0 256 256` canvas, asserted by a test, which is what makes equal boxes give equal optical weight —
the property entry 4 could not hold. Moving something off the hand-drawn list needs its own entry
here.

## 9. Agent tints may not reuse a reserved hue

**Status:** Accepted.

**Context:** Agent tints are the one saturated, non-status use of colour in the product (entry 2). The
original allocation collided with it: one agent wore the additions green, another wore an amber one
step from the needs-input amber it sits *beside* in a rail row, and a third wore the exact branch-scope
violet.

**Decision:** No agent tint may sit in a hue already spent on status or on diffs. The pool is
enumerable (`theme::agent::TINT_POOL`) and the rule is enforced by a real test
(`theme::agent_tint_allocation_tests`), not by review.

**Consequences:** The pool was reallocated to copper / teal / periwinkle / steel blue. Adding an agent
means adding a tint that passes the test, which in practice means the pool is finite and a fifth or
sixth agent will need a deliberate hue decision rather than the next colour to hand.

## 10. `Status::Review` renders as `Finished`

**Status:** Accepted. (GitHub issue #280.)

**Context:** The status originally rendered as `Review ready`.

**Decision:** The rendered word is `Finished`. The enum variant keeps its `Review` name — it is an
internal identifier and this is only about the string.

**Consequences:** "Review ready" states a judgement the agent cannot make: the agent knows it exited
zero with a non-empty diff, not that the work is ready for review. It also collided with the user's
*own* review progress tracked in the Changes panel, and contradicted the app's own vocabulary.
`Finished` states the fact, and the file count rendered beside it carries what there is to look at.
The urgency ordering is unchanged.

## 11. The status bar watches agents; it is not an editor's footer

**Status:** Accepted. (GitHub issue #293.)

**Context:** An audit of the status bar counted thirteen readouts and found eight of them lifted from
VS Code — cursor position, indent width, line ending, encoding, editor zoom, UI scale among them.
"VS Code's footer answers *what am I typing into*; Jerry's job is watching agents. Wrong app's
chrome."

**Decision:** Delete all eight, code paths included. What is left is three groups on three type
tiers: a transient notice slot, the branch cluster, running agents, and machine load on the left; the
environment chip and the palette hint on the right.

**Consequences:** Editor zoom survives as `mod+plus`/`mod+minus` — the state and handlers were kept,
only the readouts went. The urgency dot cluster moved into the title bar. `N worktrees · Y GB` was
dropped because the rail footer carries it 30px away: the rail owns worktree inventory and its prune
action, the bar owns activity and cost. This is also where the general rule comes from that replacing
a control means deleting its old keys in the same edit — a key defined twice is two specifications of
one thing and the reader cannot tell which is real.

## 12. The agent pane's bottom strip is a readout, not an action bar

**Status:** Accepted. (GitHub issue #295.)

**Context:** The strip below an agent pane originally carried a per-status row of git actions — keep
all, review diff, open in editor, discard, retry, interrupt, open terminal.

**Decision:** It is a readout. `work_surface::state::ActionKind` went from seven variants to two
(`Respawn`, `DiscardWorktree`); the other five were **deleted**, not hidden behind a condition.

**Consequences:** The pane reports what the agent is doing rather than competing with it for the
bottom of the screen, and the actions that survive are the ones the CLI genuinely cannot perform for
itself. Anything still rendered without backing logic is dimmed and non-interactive
(`FooterAction::implemented`) — never a clickable-looking no-op.

## 13. The palette binds `mod+P`

**Status:** Accepted.

**Context:** `mod+K` was the original binding. The file editor's real `ctrl-k ctrl-d` chord registers
`ctrl-k` as a chord *prefix* in that context, so a lone press waited out GPUI's ~1s prefix timeout
before replaying and reaching the palette.

**Decision:** Bind `mod+P`, unscoped, as a real replacement — not an alias alongside `mod+K`.

**Consequences:** No delay, and the binding matches the VS Code / Sublime convention directly. The
known tradeoff: a focused terminal's own readline `Ctrl+P` (`previous-history`) is shadowed by this
binding rather than reaching the shell. A terminal's Up-arrow history navigation is unaffected.

## 14. `docs/design/` replaces the design-handoff bundle

**Status:** Accepted. (GitHub issue #414.)

**Context:** `design_handoff_jerry_ade/` was a one-shot handoff bundle — an interactive HTML mockup, a
transcribed `tokens.rs`, and a README written as a build brief. It was the de-facto design authority:
364 doc comments across 81 files in `crates/` cited it, and so did `CONTRIBUTING.md`,
`docs/development-workflow.md`, two skills and an issue template. `CONTRIBUTING.md` also described it
honestly as "a one-time handoff artifact, not a living spec".

Three things had gone wrong. Roughly a third of those citations pointed at `revision N/` directories
that were never committed, so a contributor reading `theme.rs` hit an unresolvable path every other
screen. The citations were exactly what `CLAUDE.md`'s comment rule forbids — design history, revision
IDs, issue archaeology — and were the single largest class of violation in the codebase. And a frozen
mockup cannot absorb a design change: entry 8 silently superseded entry 4 with nothing in the
repository to record it.

**Decision:** Delete the bundle and replace it with this documentation set. `theme.rs` stays the
source of truth for **values**; these docs cover **intent, structure, vocabulary and invariants**, and
name the token rather than reprinting a hex. The bundle is preserved by git history alone — no tag,
no release asset.

**Consequences:** The docs cannot drift from the code on values, because they never duplicate them. A
UI change updates the relevant page in the same PR, and a call worth recording gets an entry here
instead of an uncommitted `revision N/` folder. Where the old mockup and the shipped app disagree,
**the app wins**, and the delta becomes an entry here or a `Not built yet` line on a surface page —
entries 7 and 10 through 13 are that reconciliation, written down for the first time.
