---
description: Run the full pre-commit gate (fmt, clippy, tests) - the same thing CI runs
model: haiku
---

Run CLAUDE.md's pre-commit gate, in order, stopping at the first failure and reporting it plainly
rather than continuing past it:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If a step fails, show the actual failing output (not a summary) and stop — don't run later steps
against code that hasn't passed the earlier ones. If everything passes, say so in one line; this
command doesn't need a report beyond pass/fail plus whatever failed.

This is the full gate, including tests. `.claude/hooks/pre-commit-check.sh` runs automatically
before `git commit` but only covers fmt + clippy (the test suite is too slow to run on every
commit) — so this command, not just a clean commit, is what "done" means before opening a PR.
