---
description: Bump the workspace version, tag, and push a Jerry release
---

Cut a release following `docs/development-workflow.md`'s "Releasing" section. Jerry has exactly
one version number (`[workspace.package].version` in the root `Cargo.toml`), and the pushed tag
must equal it or `release.yml`'s `version-check` job refuses the release.

1. Confirm the working tree is clean and on `master` (`git status --short`, `git branch
   --show-current`); stop and report if not.

2. Get the current version from `Cargo.toml` and the commit log since the last tag
   (`git describe --tags --abbrev=0`, then `git log <tag>..HEAD --oneline`). Suggest the next
   version from that log (patch for fixes, minor for features, major for breaking changes) and
   confirm it with the user before doing anything else — never bump or tag without confirmation.

3. Update `version` in `Cargo.toml`'s `[workspace.package]` to the confirmed version. Run
   `.claude/hooks/check-release-version.sh v<version>` to confirm it now matches, and `cargo build
   --workspace` to confirm the lockfile still resolves.

4. Commit (`chore: bump version to <version>`), tag (`git tag v<version>`), and push both
   (`git push && git push origin v<version>`).

5. Watch the release run: `gh run list --branch v<version> --limit 1 --json status,conclusion,url`,
   polling until it completes. Report the result — link the run on failure, link the release
   (`gh release view v<version> --json url -q .url`) on success.

Don't write a changelog or touch Linear here — this is the fast path (version bump → tag → push →
watch CI).
