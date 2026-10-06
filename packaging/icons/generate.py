#!/usr/bin/env python3
"""Generate the placeholder app icon (PNG + ICO) with the standard library only.

A dark rounded square with three offset "tracks". Replace switchyard.png with the
real artwork when it exists; the packaging scripts derive every other format from it.
"""

import struct
import zlib
from pathlib import Path

SIZE = 1024
BG = (24, 26, 33)
ACCENT = [(94, 129, 244), (64, 196, 160), (240, 170, 70)]


def inside_rounded(x, y, size, radius):
    cx = min(max(x, radius), size - 1 - radius)
    cy = min(max(y, radius), size - 1 - radius)
    return (x - cx) ** 2 + (y - cy) ** 2 <= radius**2


def render(size):
    margin = size // 10
    inner = size - 2 * margin
    radius = inner // 5
    bar_h = inner // 9
    bars = [
        (margin + inner * 0.18, margin + inner * 0.28, margin + inner * 0.70),
        (margin + inner * 0.18 + inner * 0.12, margin + inner * 0.50, margin + inner * 0.82),
        (margin + inner * 0.18, margin + inner * 0.72, margin + inner * 0.62),
    ]
    rows = []
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            px = (0, 0, 0, 0)
            lx, ly = x - margin, y - margin
            if 0 <= lx < inner and 0 <= ly < inner and inside_rounded(lx, ly, inner, radius):
                px = (*BG, 255)
                for (x0, yc, x1), color in zip(bars, ACCENT):
                    r = bar_h / 2
                    if yc - r <= y <= yc + r and x0 - r <= x <= x1 + r:
                        nx = min(max(x, x0), x1)
                        if (x - nx) ** 2 + (y - yc) ** 2 <= r * r:
                            px = (*color, 255)
            row.extend(px)
        rows.append(bytes(row))
    return b"".join(rows)


def png(size):
    def chunk(tag, data):
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(render(size), 9))
        + chunk(b"IEND", b"")
    )


def ico(sizes):
    images = [png(s) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    for s, data in zip(sizes, images):
        dim = 0 if s >= 256 else s
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return header + entries + b"".join(images)


if __name__ == "__main__":
    here = Path(__file__).parent
    (here / "switchyard.png").write_bytes(png(SIZE))
    (here / "switchyard-256.png").write_bytes(png(256))
    (here / "switchyard.ico").write_bytes(ico([16, 32, 48, 256]))
    print("wrote switchyard.png, switchyard-256.png and switchyard.ico")
