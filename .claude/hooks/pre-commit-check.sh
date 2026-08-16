#!/usr/bin/env bash
# PreToolUse:Bash hook. Runs CLAUDE.md's pre-commit gate - the structural ratchet
# (check-conventions.sh), fmt --check, and clippy -D warnings - before a `git commit` command
# executes, so a lint-failing or convention-regressing change never lands in history.
# `cargo test --workspace` is intentionally not part of this gate (or `/check`'s) right now -
# GitHub issue #348 tracks why and getting it back. Run tests relevant to your change manually.
#
# Exit code contract, matching Claude Code's PreToolUse protocol: this hook must NEVER hard-fail
# in a way that blocks work for a reason unrelated to the gate itself. Missing `jq`, missing
# `cargo`, or a command that isn't a git commit all exit 0 (allow, unmodified) rather than error.
# Only a genuine fmt/clippy failure produces a deny decision.

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

if [ -n "$FAIL" ]; then
  if [ "$FAIL" = "conventions" ]; then
    REASON="the convention ratchet failed - see /tmp/jerry-precommit-conventions.log."
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
