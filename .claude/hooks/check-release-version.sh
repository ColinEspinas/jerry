#!/usr/bin/env bash
# Guards against crate versions drifting from the release tag (issue #317), which silently
# broke the in-app updater's version comparison. Checks that every crates/*/Cargo.toml inherits
# `version` from the workspace, and - if $1 is set to a tag - that the tag matches it.
#
# Usage:
#   .claude/hooks/check-release-version.sh          (run from the repo root; inheritance only)
#   .claude/hooks/check-release-version.sh v0.1.0    (also checks the tag against the version)

set -u

cd "${CLAUDE_PROJECT_DIR:-.}" || exit 1

ROOT_MANIFEST="Cargo.toml"
fail=0

workspace_version=$(grep -A20 '^\[workspace.package\]' "$ROOT_MANIFEST" | grep -m1 '^version' | sed -E 's/version = "([^"]+)".*/\1/')
if [ -z "$workspace_version" ]; then
  echo "check-release-version: FAIL - could not find a version in $ROOT_MANIFEST's [workspace.package]." >&2
  exit 1
fi

offenders=$(grep -lE '^version = "[^"]+"' crates/*/Cargo.toml 2>/dev/null)
if [ -n "$offenders" ]; then
  echo "check-release-version: FAIL - these crate manifests pin their own version instead of inheriting from the workspace:" >&2
  echo "$offenders" | sed 's/^/  /' >&2
  echo "  Use version = { workspace = true }, matching edition/publish/license in the same file." >&2
  fail=1
fi

tag="${1:-}"
if [ -n "$tag" ]; then
  tag_version="${tag#v}"
  tag_version="${tag_version#V}"
  if [ "$tag_version" != "$workspace_version" ]; then
    echo "check-release-version: FAIL - tag '$tag' (version $tag_version) does not match the workspace version '$workspace_version' in $ROOT_MANIFEST." >&2
    echo "  Bump [workspace.package] version to match before tagging, or fix the tag." >&2
    fail=1
  fi
fi

if [ "$fail" -eq 0 ]; then
  if [ -n "$tag" ]; then
    echo "check-release-version: OK (workspace version $workspace_version matches tag $tag)"
  else
    echo "check-release-version: OK (all crates inherit workspace version $workspace_version)"
  fi
fi

exit $fail
