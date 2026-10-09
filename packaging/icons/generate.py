#!/usr/bin/env python3
"""Render the Switchyard app icon set from the SVG sources in this folder.

    pip install cairosvg pillow
    python3 packaging/icons/generate.py

Sources (checked in, edit these):
  switchyard.svg        full artwork, used for 48 px and up
  switchyard-small.svg  heavier strokes and no detail, used for 16-32 px

Outputs (checked in, so packaging needs neither tool):
  switchyard.png        1024 px, full-bleed tile
  switchyard-256.png    AppImage / desktop icon
  png/switchyard-<n>.png  hicolor sizes for Linux packages
  switchyard.ico        Windows: installer, shortcuts and the exe resource (crates/app/build.rs)
  switchyard.icns       macOS bundle icon, on Apple's icon grid (824 px tile in 1024)
"""

import io
import struct
from pathlib import Path

import cairosvg
from PIL import Image, ImageFilter

HERE = Path(__file__).resolve().parent
SMALL_MAX = 32
HICOLOR = [16, 24, 32, 48, 64, 128, 256, 512]
ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 256]


def render(size):
    """The full-bleed tile at `size` px, from the source suited to that size."""
    source = HERE / ("switchyard-small.svg" if size <= SMALL_MAX else "switchyard.svg")
    data = cairosvg.svg2png(url=str(source), output_width=size, output_height=size)
    return Image.open(io.BytesIO(data)).convert("RGBA")


def png_bytes(image):
    out = io.BytesIO()
    image.save(out, format="PNG", optimize=True)
    return out.getvalue()


def mac_tile(size):
    """Apple's grid: the tile is 824/1024 of the canvas, centred, with a soft shadow."""
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    tile_px = round(size * 824 / 1024)
    tile = render(tile_px)
    offset = (size - tile_px) // 2
    if size >= 64:
        alpha = tile.getchannel("A").point(lambda a: a * 0.35)
        shadow = Image.new("RGBA", tile.size, (0, 0, 0, 0))
        shadow.putalpha(alpha)
        pad = Image.new("RGBA", (size, size), (0, 0, 0, 0))
        pad.alpha_composite(shadow, (offset, offset + max(1, size // 100)))
        canvas = pad.filter(ImageFilter.GaussianBlur(max(1, size / 80)))
    canvas.alpha_composite(tile, (offset, offset))
    return canvas


def dib_bytes(image):
    """A 32-bit BGRA DIB icon image (BITMAPINFOHEADER, pixels bottom-up, then the AND mask)."""
    w, h = image.size
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    rows = []
    for y in reversed(range(h)):
        row = bytearray()
        for x in range(w):
            r, g, b, a = image.getpixel((x, y))
            row += bytes((b, g, r, a))
        rows.append(bytes(row))
    mask_row = ((w + 31) // 32) * 4
    return header + b"".join(rows) + b"\0" * (mask_row * h)


def ico(sizes):
    """ICO with DIB entries below 256 px (what every Windows icon loader reads) and a PNG
    entry at 256 px."""
    images = [png_bytes(render(s)) if s >= 256 else dib_bytes(render(s)) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    for s, data in zip(sizes, images):
        dim = 0 if s >= 256 else s
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    return header + entries + b"".join(images)


def icns():
    """ICNS with PNG entries for every slot `iconutil` would write."""
    slots = [
        (b"icp4", 16), (b"icp5", 32), (b"ic11", 32), (b"ic12", 64), (b"ic07", 128),
        (b"ic13", 256), (b"ic08", 256), (b"ic14", 512), (b"ic09", 512), (b"ic10", 1024),
    ]
    cache = {}
    body = b""
    for tag, size in slots:
        if size not in cache:
            cache[size] = png_bytes(mac_tile(size))
        data = cache[size]
        body += tag + struct.pack(">I", 8 + len(data)) + data
    return b"icns" + struct.pack(">I", 8 + len(body)) + body


def main():
    render(1024).save(HERE / "switchyard.png", optimize=True)
    render(256).save(HERE / "switchyard-256.png", optimize=True)
    png_dir = HERE / "png"
    png_dir.mkdir(exist_ok=True)
    for s in HICOLOR:
        render(s).save(png_dir / f"switchyard-{s}.png", optimize=True)
    (HERE / "switchyard.ico").write_bytes(ico(ICO_SIZES))
    (HERE / "switchyard.icns").write_bytes(icns())
    print("wrote switchyard.png, switchyard-256.png, png/, switchyard.ico and switchyard.icns")


if __name__ == "__main__":
    main()
