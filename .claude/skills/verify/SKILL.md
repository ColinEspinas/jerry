---
name: verify
description: Launch Jerry, screenshot the running window, and check the result against the design spec in a loop - the only way to give an agent actual eyes on the UI instead of shipping layout/visual changes blind. Use whenever the user asks to verify a UI change visually, check how something looks, take a screenshot of the app, confirm a fix worked in the real app, or before opening a PR for anything touching render.rs, theme.rs, or layout. Not for logic-only changes with no visual surface - a passing test suite already covers those.
---

# Verify

Nothing in this project gives an agent eyes on the running app by default — UI work gets written,
compiled, and shipped without anyone (human or agent) having actually looked at it. This skill
closes that loop: launch, screenshot, compare against a real oracle, iterate.

## One-time setup this skill cannot do for you

The terminal running Claude Code needs **Screen Recording** and **Accessibility** permissions
(System Settings → Privacy & Security) before `screencapture`/`osascript` can see anything. Neither
can be granted programmatically. If a capture comes back solid black or empty, that's the signature
of a missing permission, not a rendering bug — stop and tell the user exactly which toggle to flip
rather than looping on garbage images and drawing conclusions from them.

## The loop

1. **Launch.** `cargo run -p app <repo-path>` in the background. Use the debug profile for
   layout/visual iteration — it compiles far faster than `--release`, and layout correctness
   doesn't depend on optimization level. Switch to `--release` only when the thing being checked is
   about feel (animation smoothness, frame timing) or performance.

2. **Find the window.** `osascript` against System Events can return the app's window bounds:

   ```applescript
   tell application "System Events" to tell (first process whose name is "app")
       get {position, size} of front window
   end tell
   ```

3. **Capture.** `screencapture -x -R<x,y,w,h> .claude/scratch/<name>.png` — the `-R` region flag
   takes the bounds from the previous step, `-x` suppresses the capture sound. `.claude/scratch/`
   is already gitignored, so captures never end up in a commit.

4. **Look at it.** Read the PNG back directly — it renders as an image, not a file listing.

5. **Compare against an oracle, not impression.** `docs/design/` carries the rules and invariants
   for each surface, and `crates/app/src/theme.rs` carries the exact values behind the tokens those
   pages name — a page says "the status pill's background is `theme::status::*_BG`", the token says
   what that is. If the task is a bug fix rather than a new UI, the issue's acceptance criteria is
   the oracle instead. Either way, name
   the specific mismatch ("the sidebar row height reads as 32px, the spec says 36px") rather than
   asserting it "looks right" or "looks off" — a vague impression isn't a finding anyone can act on.

6. **Drive the UI when the default view isn't the target.** `osascript` can send keystrokes/clicks
   to reach a specific surface (open settings, trigger the command palette, switch tabs) before
   recapturing.

7. **Iterate**: fix → rebuild → recapture. GPUI rebuilds run into minutes even in debug, so batch
   several related edits into one recompile rather than recapturing after every single change.

8. **Attach the final capture** to the PR (`ship`'s step 2) instead of describing the result in
   prose — the whole point of this skill is replacing "should look right" with a real image.
   Captures are never committed to the repo on their own — `.claude/scratch/` is gitignored, and a
   PR attachment or an inline verification in this session is the only place one belongs.
