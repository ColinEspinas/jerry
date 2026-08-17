#!/usr/bin/env python3
"""Packs already-resized PNGs into a single multi-resolution Windows .ico.

Stdlib only, on purpose: the release runners and a contributor's machine both need to be able
to regenerate the icon set, and neither ImageMagick nor Pillow is a reasonable thing to require
for a file that changes roughly never. The ICO container is a header plus one directory entry
per image, and every Windows version this project targets reads PNG-compressed entries at every
size, so each entry's payload is just the source PNG copied in verbatim.

Usage: png-to-ico.py <out.ico> <16.png> <32.png> ...
"""

import struct
import sys
from pathlib import Path

# ICONDIR: reserved(0), type(1 = icon), image count.
ICONDIR = "<HHH"
# ICONDIRENTRY: width, height, palette count, reserved, colour planes, bpp, size, offset.
# Width/height are single bytes, where 0 means 256 - the reason ICO tops out there.
ICONDIRENTRY = "<BBBBHHII"


def png_size(data: bytes) -> tuple[int, int]:
    """Reads width/height out of a PNG's IHDR, which is always the first chunk."""
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise ValueError("not a PNG")
    width, height = struct.unpack(">II", data[16:24])
    return width, height


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2

    out_path = Path(sys.argv[1])
    images = []
    for arg in sys.argv[2:]:
        data = Path(arg).read_bytes()
        width, height = png_size(data)
        if width > 256 or height > 256:
            raise ValueError(f"{arg}: {width}x{height} exceeds ICO's 256px maximum")
        images.append((width, height, data))

    images.sort(key=lambda image: image[0])

    header = struct.pack(ICONDIR, 0, 1, len(images))
    offset = len(header) + len(images) * struct.calcsize(ICONDIRENTRY)

    entries = bytearray()
    payloads = bytearray()
    for width, height, data in images:
        entries += struct.pack(
            ICONDIRENTRY,
            width % 256,
            height % 256,
            0,
            0,
            1,
            32,
            len(data),
            offset,
        )
        payloads += data
        offset += len(data)

    out_path.write_bytes(header + bytes(entries) + bytes(payloads))
    print(f"{out_path}: {len(images)} sizes, {out_path.stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
