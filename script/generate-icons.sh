#!/usr/bin/env bash
# Regenerates every platform icon format from the one master PNG.
#
# The generated files are committed rather than built in CI: the release runners then need no
# extra tooling, and the master artwork changes roughly never. Run this only when
# `crates/app/resources/app-icon.png` itself changes, and commit what it writes.
#
# macOS-only (`sips`/`iconutil` ship with the OS and have no cross-platform equivalent worth
# depending on). That's fine for a maintainer-run regeneration step - nothing in the build or
# the release workflow calls this.
set -euo pipefail

cd "$(dirname "$0")/.."

readonly MASTER="crates/app/resources/app-icon.png"
readonly MACOS_DIR="crates/app/resources/macos"
readonly WINDOWS_DIR="crates/app/resources/windows"
readonly LINUX_DIR="crates/app/resources/linux"

if [[ ! -f "$MASTER" ]]; then
    echo "error: $MASTER not found" >&2
    exit 1
fi

if ! command -v iconutil >/dev/null 2>&1; then
    echo "error: iconutil not found - this script only runs on macOS" >&2
    exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# `sips -Z` fits the image into a square of the given size, preserving aspect ratio. The master
# is already square, so this is a plain resize.
resize() {
    sips -s format png -Z "$1" "$MASTER" --out "$2" >/dev/null
}

echo "==> macOS: Jerry.icns"
iconset="$work/Jerry.iconset"
mkdir -p "$iconset"
# The exact filenames `iconutil` expects; anything else in the directory makes it fail.
for size in 16 32 128 256 512; do
    resize "$size" "$iconset/icon_${size}x${size}.png"
    resize "$((size * 2))" "$iconset/icon_${size}x${size}@2x.png"
done
mkdir -p "$MACOS_DIR"
iconutil --convert icns "$iconset" --output "$MACOS_DIR/Jerry.icns"

echo "==> Windows: app-icon.ico"
mkdir -p "$WINDOWS_DIR"
ico_pngs=()
for size in 16 24 32 48 64 128 256; do
    resize "$size" "$work/ico-${size}.png"
    ico_pngs+=("$work/ico-${size}.png")
done
python3 script/png-to-ico.py "$WINDOWS_DIR/app-icon.ico" "${ico_pngs[@]}"

echo "==> Linux: hicolor PNGs"
mkdir -p "$LINUX_DIR"
for size in 128 256 512; do
    resize "$size" "$LINUX_DIR/app-icon-${size}.png"
done
cp "$MASTER" "$LINUX_DIR/app-icon-1024.png"

echo
echo "Generated:"
find "$MACOS_DIR" "$WINDOWS_DIR" "$LINUX_DIR" -type f | sort
