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


GRADIENT = ((0xF9, 0x73, 0x16), (0xDC, 0x26, 0x26))  # orange -> red
SPARKLE = (0xFA, 0xCC, 0x15)


def sparkle(cx, cy, r, n=48):
    """Four-point star whose edges are the SVG's quadratic curves: sqrt|u|+sqrt|v|=1."""
    pts = []
    for qx, qy in ((1, -1), (1, 1), (-1, 1), (-1, -1)):
        for i in range(n):
            t = i / n
            a, b = t * t, (1 - t) ** 2  # from the vertical tip to the horizontal tip
            if qx * qy > 0:
                a, b = b, a
            pts.append((cx + qx * r * a, cy + qy * r * b))
    return pts


def render(size: int) -> Image.Image:
    big = size * S
    k = big / 512.0  # the design uses a 512x512 grid (magic-* icon family)

    def r(*v):
        return [round(x * k) for x in v]

    # Diagonal gradient, clipped to the rounded tile.
    grad = Image.new("RGBA", (big, big))
    gp = grad.load()
    (r0, g0, b0), (r1, g1, b1) = GRADIENT
    for y in range(big):
        for x in range(big):
            t = (x + y) / (2 * big)
            gp[x, y] = (round(r0 + (r1 - r0) * t), round(g0 + (g1 - g0) * t), round(b0 + (b1 - b0) * t), 255)
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle(r(0, 0, 512, 512), radius=round(112 * k), fill=255)
    img = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    img.paste(grad, (0, 0), mask)

    layer = Image.new("RGBA", (big, big), (0, 0, 0, 0))
    d = ImageDraw.Draw(layer)
    # Three pages side by side: the horizontal layout.
    for x, alpha in ((64, 255), (200, 255), (336, 179)):
        d.rounded_rectangle(r(x, 196, x + 112, 352), radius=round(12 * k), fill=(255, 255, 255, alpha))
    for x in (84, 220):
        for y, w in ((228, 72), (256, 56), (284, 72)):
            d.rounded_rectangle(r(x, y, x + w, y + 12), radius=round(6 * k), fill="#fdba74")
    d.polygon([(x * k, y * k) for x, y in sparkle(404, 112, 66)], fill=SPARKLE)
    img = Image.alpha_composite(img, layer)
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
