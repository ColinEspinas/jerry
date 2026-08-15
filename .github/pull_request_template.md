<!-- Closes #<issue> — delete this line entirely if there's no tracked issue. -->

## What

<!-- What changed, from a reviewer's point of view - what breaks or works differently now that
     didn't before. Not a restatement of the commit list or the file names touched. If `plan`
     already posted an approach on the issue, this can be short - the issue carries the reasoning.
     Scale this to the change: a one-line fix gets a one-line summary, not padding. -->

## Why

<!-- Only if it's not obvious from the issue/title - what this unblocks, or what was broken/awkward
     without it. Skip this section entirely for a self-explanatory fix. -->

## Testing

- [ ] `/check` (fmt + clippy `-D warnings` + `cargo test --workspace`) passes locally
- [ ] `verify` capture attached below, if this touches `render.rs`/`theme.rs`/layout
- [ ] New/changed behavior has a real test, not just a compile check

<!-- Note anything that couldn't be run and why ("not run: needs a live rust-analyzer") rather
     than silently leaving a box unchecked. -->

## Architecture notes

<!-- Only if this touches the crate boundary or the Command/Query shape - e.g. "new Command:
     AttemptCherryPick in wt-core", "extracted X into its own crate per docs/architecture/decisions.md §N". Delete
     this section for a change that doesn't touch any of that. -->

## Screenshot

<!-- For UI-visible changes only - the `verify` skill's capture. Delete this section for a
     logic-only change rather than leaving it empty. -->
