---
description: Run the full pre-commit gate (conventions, fmt, clippy, tests) - the same thing CI runs
model: haiku
---

Run CLAUDE.md's pre-commit gate, in order, stopping at the first failure and reporting it plainly
rather than continuing past it:

```sh
.claude/hooks/check-conventions.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
```

If a step fails, show the actual failing output (not a summary) and stop — don't run later steps
against code that hasn't passed the earlier ones. If everything passes, say so in one line; this
command doesn't need a report beyond pass/fail plus whatever failed.

The test step covers the `unit` and `ui` tiers, which is what CI's `Linux (test)` job runs. The
`external` tier is `#[ignore]`d and skipped — it needs real language servers and runs only in the
nightly `External` job (see [`docs/testing.md`](../../docs/testing.md)). If you changed something
that tier covers, run it yourself with `cargo nextest run --workspace --profile external
--run-ignored all` and the servers installed.

If `cargo nextest` isn't installed, run `/setup` — don't fall back to `cargo test`, which has no
per-test timeout and will simply hang on a blocked test instead of naming it.
