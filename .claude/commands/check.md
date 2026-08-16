---
description: Run the full pre-commit gate (conventions, fmt, clippy) - the same thing CI runs
model: haiku
---

Run CLAUDE.md's pre-commit gate, in order, stopping at the first failure and reporting it plainly
rather than continuing past it:

```sh
.claude/hooks/check-conventions.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

If a step fails, show the actual failing output (not a summary) and stop — don't run later steps
against code that hasn't passed the earlier ones. If everything passes, say so in one line; this
command doesn't need a report beyond pass/fail plus whatever failed.

**`cargo test --workspace` is deliberately not part of this gate right now** — see
[GitHub issue #348](https://github.com/ColinEspinas/jerry/issues/348), which tracks getting the
suite back into CI and this gate once resolved. If your change needs test coverage verified, run
the relevant scoped tests yourself (e.g. `cargo test -p wt-core`, or `cargo test -p app --lib
<module>::`) — don't treat a clean `/check` as proof the full suite passes.
