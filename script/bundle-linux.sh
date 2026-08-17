#!/usr/bin/env bash
# Assembles dist/jerry-linux.tar.gz - a real launcher entry (.desktop + hicolor icons), not a
# bare executable, and an install.sh that puts both where a user's desktop environment expects to
# find them (GitHub issue #447 bullet 1/3: no console window flash, real Applications-menu entry).
set -euo pipefail

cd "$(dirname "$0")/.."

readonly APP_NAME="Jerry"
readonly APP_CLI="jerry"
export APP_CLI
readonly BIN_NAME="app"
readonly RESOURCES_DIR="crates/app/resources/linux"
readonly DIST_DIR="dist"
readonly STAGE_DIR="${DIST_DIR}/jerry"
readonly ARCHIVE_PATH="${DIST_DIR}/jerry-linux.tar.gz"

# Single source of truth for the release version, matching how
# .claude/hooks/check-release-version.sh reads it: the `version` under Cargo.toml's
# [workspace.package] table. Not otherwise used by this script (the tarball itself carries no
# version string), kept only so a future need for it doesn't require re-deriving the parse.
VERSION=$(grep -A20 '^\[workspace.package\]' Cargo.toml | grep -m1 '^version' | sed -E 's/version = "([^"]+)".*/\1/')
if [[ -z "$VERSION" ]]; then
    echo "error: could not read [workspace.package] version from Cargo.toml" >&2
    exit 1
fi
echo "==> Bundling ${APP_NAME} ${VERSION} for Linux"

if ! command -v envsubst >/dev/null 2>&1; then
    echo "error: envsubst not found (part of gettext - 'gettext-base' on Debian/Ubuntu, 'gettext' via Homebrew)" >&2
    exit 1
fi

readonly RELEASE_BIN="target/release/${BIN_NAME}"
if [[ -f "$RELEASE_BIN" && -n "${SKIP_BUILD:-}" ]]; then
    echo "==> SKIP_BUILD set and ${RELEASE_BIN} exists - reusing it"
else
    if [[ -f "$RELEASE_BIN" ]]; then
        echo "==> ${RELEASE_BIN} already exists - reusing it (set SKIP_BUILD=1 to make this explicit, or remove it to force a rebuild)"
    else
        echo "==> Building ${RELEASE_BIN}"
        cargo build --release -p app
    fi
fi

if [[ ! -f "$RELEASE_BIN" ]]; then
    echo "error: ${RELEASE_BIN} not found after build step" >&2
    exit 1
fi

echo "==> Assembling ${STAGE_DIR}"
rm -rf "$STAGE_DIR"
mkdir -p "${STAGE_DIR}/bin" "${STAGE_DIR}/share/applications"

cp "$RELEASE_BIN" "${STAGE_DIR}/bin/${APP_CLI}"
chmod +x "${STAGE_DIR}/bin/${APP_CLI}"

echo "==> Icons"
for size in 128 256 512 1024; do
    icon_dir="${STAGE_DIR}/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$icon_dir"
    cp "${RESOURCES_DIR}/app-icon-${size}.png" "${icon_dir}/jerry.png"
done

echo "==> Desktop entry"
envsubst '$APP_CLI' \
    < "${RESOURCES_DIR}/jerry.desktop.in" \
    > "${STAGE_DIR}/share/applications/jerry.desktop"
chmod +x "${STAGE_DIR}/share/applications/jerry.desktop"

echo "==> install.sh"
cat > "${STAGE_DIR}/install.sh" <<'INSTALL_SH'
#!/usr/bin/env bash
# Installs Jerry into ~/.local - no root required. Idempotent: running this again after an
# update just overwrites the same files.
set -euo pipefail

cd "$(dirname "$0")"

readonly PREFIX="${HOME}/.local"
readonly BIN_DIR="${PREFIX}/bin"
readonly APPLICATIONS_DIR="${PREFIX}/share/applications"
readonly ICONS_DIR="${PREFIX}/share/icons/hicolor"

mkdir -p "$BIN_DIR" "$APPLICATIONS_DIR"
install -m 755 bin/jerry "${BIN_DIR}/jerry"
echo "installed ${BIN_DIR}/jerry"

install -m 644 share/applications/jerry.desktop "${APPLICATIONS_DIR}/jerry.desktop"
echo "installed ${APPLICATIONS_DIR}/jerry.desktop"

for size_dir in share/icons/hicolor/*/apps; do
    size="$(basename "$(dirname "$size_dir")")"
    dest="${ICONS_DIR}/${size}/apps"
    mkdir -p "$dest"
    install -m 644 "${size_dir}/jerry.png" "${dest}/jerry.png"
    echo "installed ${dest}/jerry.png"
done

# Both are optional: not every distro ships them, and a missing cache just means the new entry/
# icon shows up after the next login rather than immediately.
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS_DIR"
    echo "refreshed the desktop database"
else
    echo "update-desktop-database not found - skipping (the launcher entry still works, it just \
may not appear until your next login)"
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$ICONS_DIR" 2>/dev/null || true
    echo "refreshed the GTK icon cache"
else
    echo "gtk-update-icon-cache not found - skipping (the icon still works, it just may not \
appear until your next login)"
fi

echo "done - Jerry should now appear in your application menu"
INSTALL_SH
chmod +x "${STAGE_DIR}/install.sh"

echo "==> Creating ${ARCHIVE_PATH}"
rm -f "$ARCHIVE_PATH"
tar -czf "$ARCHIVE_PATH" -C "$DIST_DIR" jerry

echo "==> Done"
echo "    ${ARCHIVE_PATH}"
