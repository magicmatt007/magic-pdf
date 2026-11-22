#!/usr/bin/env python3
"""Generates scanned.pdf: two image-only pages (no text layer) for the OCR test.

Page 1 is an upright A4 page. Page 2 is stored landscape with /Rotate 90 and its
image turned the other way, so it also displays upright; it checks that the text
layer follows the page's own rotation.

Needs Pillow (python3 -m pip install pillow): python3 tests/fixtures/make_scanned.py
"""
import os
import zlib

from PIL import Image, ImageDraw, ImageFont

HERE = os.path.dirname(os.path.abspath(__file__))
FONT = os.path.join(HERE, "..", "..", "assets", "fonts", "noto-sans", "NotoSans-Regular.ttf")
DPI = 200
PAGE = (595, 842)  # A4 portrait, points

LINES = [
    "Scanned letter",
    "",
    "The quick brown fox jumps over the lazy dog.",
    "Magic PDF recognises text in scanned pages.",
    "Invoice number 4711 is due on Friday.",
]


def scan(title):
    w, h = (round(v * DPI / 72) for v in PAGE)
    img = Image.new("L", (w, h), 255)
    draw = ImageDraw.Draw(img)
    big = ImageFont.truetype(FONT, 72)
    body = ImageFont.truetype(FONT, 40)
    y = 200
    for i, line in enumerate([title] + LINES[1:]):
        draw.text((160, y), line, font=big if i == 0 else body, fill=0)
        y += 110 if i == 0 else 70
    return img


def image_obj(img):
    data = zlib.compress(img.tobytes(), 9)
    head = (f"<< /Type /XObject /Subtype /Image /Width {img.width} /Height {img.height} "
            f"/ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode "
            f"/Length {len(data)} >>\nstream\n").encode()
    return head + data + b"\nendstream"


def stream_obj(text):
    data = text.encode()
    return f"<< /Length {len(data)} >>\nstream\n".encode() + data + b"\nendstream"


def build():
    page1 = scan("Scanned letter, page one")
    # Rotating the image a quarter turn counter-clockwise; /Rotate 90 turns it back.
    page2 = scan("Scanned letter, page two").rotate(90, expand=True)
    w, h = PAGE
    objs = {
        1: b"<< /Type /Catalog /Pages 2 0 R >>",
        2: b"<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 >>",
        3: (f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] "
            f"/Resources << /XObject << /Im1 4 0 R >> >> /Contents 5 0 R >>").encode(),
        4: image_obj(page1),
        5: stream_obj(f"q {w} 0 0 {h} 0 0 cm /Im1 Do Q"),
        6: (f"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {h} {w}] /Rotate 90 "
            f"/Resources << /XObject << /Im1 7 0 R >> >> /Contents 8 0 R >>").encode(),
        7: image_obj(page2),
        8: stream_obj(f"q {h} 0 0 {w} 0 0 cm /Im1 Do Q"),
    }
    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = {}
    for num in sorted(objs):
        offsets[num] = len(out)
        out += f"{num} 0 obj\n".encode() + objs[num] + b"\nendobj\n"
    xref = len(out)
    out += f"xref\n0 {len(objs) + 1}\n0000000000 65535 f \n".encode()
    for num in sorted(objs):
        out += f"{offsets[num]:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {len(objs) + 1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


if __name__ == "__main__":
    path = os.path.join(HERE, "scanned.pdf")
    with open(path, "wb") as f:
        f.write(build())
    print(f"wrote {path}")
