#!/usr/bin/env bash
# Assembles dist/Jerry.app and dist/Jerry-macos.dmg from a release build.
#
# Hand-rolled rather than cargo-bundle: the bundle is three files (binary, icon, Info.plist),
# and cargo-bundle upstream is stale enough that Zed pins its own fork to keep using it
# (`cargo-bundle v0.6.1-zed`). CLAUDE.md's dependency rule prefers the shell over the fork here.
set -euo pipefail

cd "$(dirname "$0")/.."

readonly APP_NAME="Jerry"
readonly BIN_NAME="app"
readonly EXECUTABLE_NAME="jerry"
readonly RESOURCES_DIR="crates/app/resources/macos"
readonly DIST_DIR="dist"
readonly APP_BUNDLE="${DIST_DIR}/${APP_NAME}.app"
readonly DMG_PATH="${DIST_DIR}/${APP_NAME}-macos.dmg"

# Single source of truth for the release version, matching how
# .claude/hooks/check-release-version.sh reads it: the `version` under Cargo.toml's
# [workspace.package] table.
VERSION=$(grep -A20 '^\[workspace.package\]' Cargo.toml | grep -m1 '^version' | sed -E 's/version = "([^"]+)".*/\1/')
if [[ -z "$VERSION" ]]; then
    echo "error: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi
echo "==> Bundling ${APP_NAME} ${VERSION}"

# CI already runs `cargo build --release -p app` as its own step; SKIP_BUILD lets it hand this
# script an already-built binary instead of paying for a second build.
readonly RELEASE_BIN="target/release/${BIN_NAME}"
if [[ -n "${SKIP_BUILD:-}" ]]; then
    echo "==> SKIP_BUILD set - expecting ${RELEASE_BIN} to already exist"
elif [[ -f "$RELEASE_BIN" ]]; then
    echo "==> ${RELEASE_BIN} already exists - reusing it"
else
    echo "==> Building ${RELEASE_BIN}"
    cargo build --release -p app
fi

if [[ ! -f "$RELEASE_BIN" ]]; then
    echo "error: ${RELEASE_BIN} not found" >&2
    exit 1
fi

echo "==> Assembling ${APP_BUNDLE}"
rm -rf "$APP_BUNDLE"
mkdir -p "${APP_BUNDLE}/Contents/MacOS" "${APP_BUNDLE}/Contents/Resources"

cp "$RELEASE_BIN" "${APP_BUNDLE}/Contents/MacOS/${EXECUTABLE_NAME}"
chmod +x "${APP_BUNDLE}/Contents/MacOS/${EXECUTABLE_NAME}"

cp "${RESOURCES_DIR}/Jerry.icns" "${APP_BUNDLE}/Contents/Resources/Jerry.icns"

cp "${RESOURCES_DIR}/Info.plist" "${APP_BUNDLE}/Contents/Info.plist"
plutil -replace CFBundleShortVersionString -string "$VERSION" "${APP_BUNDLE}/Contents/Info.plist"
plutil -replace CFBundleVersion -string "$VERSION" "${APP_BUNDLE}/Contents/Info.plist"

echo "==> Code signing (identity: ${MACOS_SIGNING_IDENTITY:-ad-hoc})"
# Ad-hoc by default (`-`); MACOS_SIGNING_IDENTITY lets a future paid Developer ID certificate
# (issue #449) drop in without editing this script. Same fallback shape as Zed's
# script/bundle-mac (`${MACOS_SIGNING_KEY:- -}`).
codesign --deep --force --sign "${MACOS_SIGNING_IDENTITY:--}" "$APP_BUNDLE"

echo "==> Verifying code signature"
if ! codesign --verify --deep --strict "$APP_BUNDLE"; then
    echo "error: codesign verification failed for ${APP_BUNDLE}" >&2
    exit 1
fi

echo "==> Creating ${DMG_PATH}"
rm -f "$DMG_PATH"
dmg_staging=$(mktemp -d)
trap 'rm -rf "$dmg_staging"' EXIT

cp -R "$APP_BUNDLE" "${dmg_staging}/${APP_NAME}.app"
ln -s /Applications "${dmg_staging}/Applications"

hdiutil create -volname "$APP_NAME" -srcfolder "$dmg_staging" -ov -format UDZO "$DMG_PATH"

echo "==> Done"
echo "    ${APP_BUNDLE}"
echo "    ${DMG_PATH}"
