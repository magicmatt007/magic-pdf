#!/usr/bin/env python3
"""Renders the app icon (same design as packaging/linux/magic-pdf.svg) into the
files the build uses. Needs Pillow: python3 assets/icons/make_icons.py

Outputs (committed, so builds need no Python):
  magic-pdf.ico        Windows exe / installer / file association icon
  magic-pdf-256.png    for docs and stores
  magic-pdf-64.rgba    raw 64x64 RGBA pixels for the window icon
"""
import os
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
S = 8  # supersampling factor for smooth edges


def render(size: int) -> Image.Image:
    big = size * S
    k = big / 128.0  # the design uses a 128x128 grid

    def r(*v):
        return [round(x * k) for x in v]

    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle(r(4, 4, 124, 124), radius=round(24 * k), fill="#1f4e9c")
    # Three pages side by side: the horizontal layout.
    for x, alpha in ((16, 255), (50, 255), (84, 191)):
        d.rounded_rectangle(r(x, 34, x + 28, 74), radius=round(3 * k), fill=(255, 255, 255, alpha))
    for x in (21, 55):
        for y, w in ((42, 18), (49, 14), (56, 18)):
            d.rounded_rectangle(r(x, y, x + w, y + 3), radius=round(1.5 * k), fill="#9db4dc")
    # Double arrow underneath.
    yellow, width = "#ffd34d", round(6 * k)
    d.line(r(30, 94, 98, 94), fill=yellow, width=width)
    d.line(r(88, 86, 98, 94, 88, 102), fill=yellow, width=width, joint="curve")
    d.line(r(40, 86, 30, 94, 40, 102), fill=yellow, width=width, joint="curve")
    for cx, cy in ((30, 94), (98, 94), (88, 86), (88, 102), (40, 86), (40, 102)):
        rad = 3 * k
        d.ellipse([cx * k - rad, cy * k - rad, cx * k + rad, cy * k + rad], fill=yellow)
    return img.resize((size, size), Image.LANCZOS)


def main():
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    big = render(256)
    big.save(os.path.join(HERE, "magic-pdf-256.png"))
    big.save(os.path.join(HERE, "magic-pdf.ico"), sizes=[(s, s) for s in sizes])
    with open(os.path.join(HERE, "magic-pdf-64.rgba"), "wb") as f:
        f.write(render(64).tobytes())
    print("icons written to", HERE)


if __name__ == "__main__":
    main()
