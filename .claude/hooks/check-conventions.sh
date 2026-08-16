#!/usr/bin/env bash
# Deterministic ratchet check for three rules this repo enforces by counting rather than by
# review. Two are the structural rules in CLAUDE.md that clippy can't express without the
# glob-import cleanup first (docs/architecture/decisions.md, entry 3): no new `use super::*`
# glob, no new adapter call from a render.rs file. The third is design_handoff_jerry_ade/
# citations in source comments (docs/design/decisions.md, entry 14): the bundle is being
# replaced by docs/design/, roughly a third of the existing citations already point at
# `revision N/` paths that were never committed, and CLAUDE.md's comment rule excludes exactly
# this material - design history, revision IDs, issue archaeology - from source comments anyway.
# This does not rely on an LLM following instructions - it's a real grep run by a real script,
# wired to three points so drift is caught as early as possible rather than only at the end:
#   1. PostToolUse:Edit|Write (settings.json) - fires right after every file edit, for the
#      fastest possible feedback. Can't block the edit that already happened, but the failing
#      exit code and message surface immediately instead of waiting for a commit attempt.
#   2. PreToolUse:Bash, nested inside pre-commit-check.sh - a real, hard block: a `git commit`
#      that would land a new violation is denied outright, not just warned about.
#   3. ci.yml - the backstop that runs regardless of what happened locally.
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
handoff_baseline=$(jq -r '.design_handoff_citations' "$BASELINE_FILE")

glob_current=$(grep -rE "use super::\*;" crates/app/src 2>/dev/null | wc -l | tr -d ' ')
adapter_current=$(grep -rE "wt_core::|pty_core::|lsp_core::|process::Command::new" crates/app/src --include='render.rs' 2>/dev/null | wc -l | tr -d ' ')
handoff_current=$(grep -rE "design_handoff|Jerry\.dc\.html" crates 2>/dev/null | wc -l | tr -d ' ')

fail=0

if [ "$glob_current" -gt "$glob_baseline" ]; then
  echo "check-conventions: FAIL - 'use super::*;' occurrences in crates/app/src rose from $glob_baseline to $glob_current." >&2
  echo "  New glob imports make layering violations unlintable (docs/architecture/decisions.md, entry 3). Use explicit imports instead." >&2
  fail=1
fi

if [ "$adapter_current" -gt "$adapter_baseline" ]; then
  echo "check-conventions: FAIL - wt_core::/pty_core::/lsp_core::/process::Command::new calls in render.rs files rose from $adapter_baseline to $adapter_current." >&2
  echo "  Render code dispatches a Command/Query instead of calling an adapter directly (docs/architecture/decisions.md, entry 3)." >&2
  fail=1
fi

if [ "$handoff_current" -gt "$handoff_baseline" ]; then
  echo "check-conventions: FAIL - design_handoff_jerry_ade/ citations in crates/ rose from $handoff_baseline to $handoff_current." >&2
  echo "  That bundle is being replaced by docs/design/ (docs/design/decisions.md, entry 14) and many of its paths were never committed. Cite docs/design/<file>.md, or drop the reference - CLAUDE.md's comment rule excludes design history from source comments." >&2
  fail=1
fi

if [ "$fail" -eq 0 ]; then
  echo "check-conventions: OK (glob imports: $glob_current/$glob_baseline, render adapter calls: $adapter_current/$adapter_baseline, handoff citations: $handoff_current/$handoff_baseline)"
fi

exit $fail
