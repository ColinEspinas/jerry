#!/usr/bin/env bash
# PreToolUse:Bash hook. Runs CLAUDE.md's pre-commit gate - the structural ratchet
# (check-conventions.sh), fmt --check, clippy -D warnings, and the `unit`/`ui` test tiers - before
# a `git commit` command executes, so a lint-failing, convention-regressing or test-breaking
# change never lands in history.
#
# Tests run through `cargo nextest`, not `cargo test`: per-test processes plus
# `.config/nextest.toml`'s `slow-timeout` mean one hung test is a named failure rather than a hook
# that never returns. The `external` tier is `#[ignore]`d and skipped by default (docs/testing.md).
#
# Exit code contract, matching Claude Code's PreToolUse protocol: this hook must NEVER hard-fail
# in a way that blocks work for a reason unrelated to the gate itself. Missing `jq`, missing
# `cargo`, missing `cargo-nextest`, or a command that isn't a git commit all exit 0 (allow,
# unmodified) rather than error. Only a genuine fmt/clippy/test failure produces a deny decision.

set -u

if ! command -v jq &>/dev/null; then
  exit 0
fi

INPUT=$(cat)
CMD=$(echo "$INPUT" | jq -r '.tool_input.command // empty')

case "$CMD" in
*'git commit'*) ;;
*) exit 0 ;;
esac

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

FAIL=""
if [ -x .claude/hooks/check-conventions.sh ]; then
  .claude/hooks/check-conventions.sh >/tmp/jerry-precommit-conventions.log 2>&1 || FAIL="conventions"
fi
if [ -z "$FAIL" ] && command -v cargo &>/dev/null; then
  cargo fmt --all -- --check >/tmp/jerry-precommit-fmt.log 2>&1 || FAIL="fmt"
fi
if [ -z "$FAIL" ] && command -v cargo &>/dev/null; then
  cargo clippy --workspace --all-targets -- -D warnings >/tmp/jerry-precommit-clippy.log 2>&1 || FAIL="clippy"
fi
# `cargo-nextest` is a separate binary, not part of a rustup toolchain: a contributor who hasn't
# run `/setup` yet skips this step rather than being blocked by it, per this file's exit contract.
if [ -z "$FAIL" ] && command -v cargo-nextest &>/dev/null; then
  cargo nextest run --workspace >/tmp/jerry-precommit-nextest.log 2>&1 || FAIL="nextest"
fi

if [ -n "$FAIL" ]; then
  if [ "$FAIL" = "conventions" ]; then
    REASON="the convention ratchet failed - see /tmp/jerry-precommit-conventions.log."
  elif [ "$FAIL" = "nextest" ]; then
    REASON="cargo nextest run --workspace failed - see /tmp/jerry-precommit-nextest.log. Run /check before committing."
  else
    REASON="cargo $FAIL failed - see /tmp/jerry-precommit-$FAIL.log. Run /check before committing."
  fi
  jq -n --arg reason "$REASON" '{
    "hookSpecificOutput": {
      "hookEventName": "PreToolUse",
      "permissionDecision": "deny",
      "permissionDecisionReason": $reason
    }
  }'
  exit 0
fi

exit 0
