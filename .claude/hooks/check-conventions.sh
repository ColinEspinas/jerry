#!/usr/bin/env bash
# Deterministic ratchet check for the two structural rules in CLAUDE.md that clippy can't
# express without the glob-import cleanup first (docs/adr/0003-ui-must-not-call-adapters.md):
# no new `use super::*` glob, no new adapter call from a render.rs file. This does not rely on
# an LLM following instructions - it's a real grep run by a real script, on every commit
# locally (via pre-commit-check.sh) and every push (via ci.yml).
#
# It is a RATCHET, not a zero-tolerance check: the codebase already has hundreds of
# pre-existing violations (tracked as backlog, not fixed in this pass), so the rule enforced
# here is "the count never goes up", not "the count is zero". Fixing an existing violation and
# lowering the corresponding number in conventions-baseline.json in the same commit is how the
# ratchet tightens over time - that's the intended way this file changes.
#
# Usage: .claude/hooks/check-conventions.sh   (run from the repo root, or set CLAUDE_PROJECT_DIR)

set -u

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 1

BASELINE_FILE=".claude/conventions-baseline.json"
if [ ! -f "$BASELINE_FILE" ]; then
  echo "check-conventions: $BASELINE_FILE missing, skipping (nothing to ratchet against)" >&2
  exit 0
fi

if ! command -v jq &>/dev/null; then
  echo "check-conventions: jq not found, skipping" >&2
  exit 0
fi

glob_baseline=$(jq -r '.glob_imports_app_src' "$BASELINE_FILE")
adapter_baseline=$(jq -r '.render_adapter_calls' "$BASELINE_FILE")

glob_current=$(grep -rE "use super::\*;" crates/app/src 2>/dev/null | wc -l | tr -d ' ')
adapter_current=$(grep -rE "wt_core::|pty_core::|lsp_core::|process::Command::new" crates/app/src --include='render.rs' 2>/dev/null | wc -l | tr -d ' ')

fail=0

if [ "$glob_current" -gt "$glob_baseline" ]; then
  echo "check-conventions: FAIL - 'use super::*;' occurrences in crates/app/src rose from $glob_baseline to $glob_current." >&2
  echo "  New glob imports make layering violations unlintable (docs/adr/0003). Use explicit imports instead." >&2
  fail=1
fi

if [ "$adapter_current" -gt "$adapter_baseline" ]; then
  echo "check-conventions: FAIL - wt_core::/pty_core::/lsp_core::/process::Command::new calls in render.rs files rose from $adapter_baseline to $adapter_current." >&2
  echo "  Render code dispatches a Command/Query instead of calling an adapter directly (docs/adr/0003)." >&2
  fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo "check-conventions: OK (glob imports: $glob_current/$glob_baseline, render adapter calls: $adapter_current/$adapter_baseline)"
fi

exit $fail
