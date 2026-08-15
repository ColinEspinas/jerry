#!/usr/bin/env bash
# PreToolUse:Bash hook. Runs the fast half of CLAUDE.md's pre-commit gate - fmt --check and
# clippy -D warnings - before a `git commit` command executes, so an unformatted or lint-failing
# change never lands in history. Deliberately does NOT run `cargo test --workspace`: this
# workspace's suite (3000+ tests, several spawning real processes/LSP servers) takes minutes,
# too slow for a hook that blocks every commit. Running the full test suite is still mandatory
# before considering work done - that's what `/check` and CLAUDE.md's own gate are for - this
# hook is a fast safety net, not a substitute for it.
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

if ! command -v cargo &>/dev/null; then
  exit 0
fi

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 0

FAIL=""
cargo fmt --all -- --check >/tmp/jerry-precommit-fmt.log 2>&1 || FAIL="fmt"
if [ -z "$FAIL" ]; then
  cargo clippy --workspace --all-targets -- -D warnings >/tmp/jerry-precommit-clippy.log 2>&1 || FAIL="clippy"
fi

if [ -n "$FAIL" ]; then
  REASON="cargo $FAIL failed - see /tmp/jerry-precommit-$FAIL.log. Run /check for the full gate (including tests) before committing."
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
