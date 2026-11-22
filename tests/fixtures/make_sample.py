#!/usr/bin/env python3
"""Generates sample.pdf: 40 pages of mixed sizes with text, links and an outline.

Pure standard library so it runs anywhere: python3 tests/fixtures/make_sample.py
"""
import os

PAGES = 40
SIZES = [(595, 842), (595, 842), (842, 595), (612, 792)]  # A4, A4, A4 landscape, Letter


def build():
    objs = {}  # num -> bytes
    n_pages = PAGES
    catalog, pages_root, font, outlines = 1, 2, 3, 4
    first_page = 10
    page_num = lambda i: first_page + i * 3       # page object
    content_num = lambda i: first_page + i * 3 + 1
    annot_num = lambda i: first_page + i * 3 + 2
    first_outline = first_page + n_pages * 3

    objs[font] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    kids = " ".join(f"{page_num(i)} 0 R" for i in range(n_pages))
    objs[pages_root] = f"<< /Type /Pages /Kids [{kids}] /Count {n_pages} >>".encode()

    for i in range(n_pages):
        w, h = SIZES[i % len(SIZES)]
        lines = [f"BT /F1 36 Tf 60 {h - 100} Td (Page {i + 1}) Tj ET"]
        y = h - 160
        para = ("The quick brown fox jumps over the lazy dog. Magic PDF horizontal layout "
                "line {k}, page {p}.")
        while y > 120:
            k = (h - 160 - y) // 18 + 1
            text = para.format(k=k, p=i + 1).replace("(", "").replace(")", "")
            lines.append(f"BT /F1 9 Tf 60 {y} Td ({text}) Tj ET")
            y -= 18
        lines.append(f"BT /F1 12 Tf 60 80 Td (Go to page 1  |  Visit example.com) Tj ET")
        # Coloured frame so rendering problems are easy to spot.
        lines.append(f"0.2 0.4 0.8 RG 4 w 20 20 {w - 40} {h - 40} re S")
        stream = "\n".join(lines).encode()
        objs[content_num(i)] = b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"\nendstream"
        objs[annot_num(i)] = (
            f"[<< /Type /Annot /Subtype /Link /Rect [58 74 150 94] /Border [0 0 0] "
            f"/Dest [{page_num(0)} 0 R /Fit] >> "
            f"<< /Type /Annot /Subtype /Link /Rect [160 74 290 94] /Border [0 0 0] "
            f"/A << /S /URI /URI (https://example.com/) >> >>]"
        ).encode()
        rotate = " /Rotate 90" if i == 5 else ""  # page 6 tests intrinsic page rotation
        objs[page_num(i)] = (
            f"<< /Type /Page /Parent {pages_root} 0 R /MediaBox [0 0 {w} {h}]{rotate} "
            f"/Resources << /Font << /F1 {font} 0 R >> >> /Contents {content_num(i)} 0 R "
            f"/Annots {annot_num(i)} 0 R >>"
        ).encode()

    # Outline: one chapter per 10 pages, each with a child for its 5th page.
    chapters = list(range(0, n_pages, 10))
    num = first_outline
    chapter_nums = [num + 2 * c for c in range(len(chapters))]
    for c, start in enumerate(chapters):
        me, child = chapter_nums[c], chapter_nums[c] + 1
        prev = f"/Prev {chapter_nums[c - 1]} 0 R " if c > 0 else ""
        nxt = f"/Next {chapter_nums[c + 1]} 0 R " if c + 1 < len(chapters) else ""
        objs[me] = (f"<< /Title (Chapter {c + 1}) /Parent {outlines} 0 R {prev}{nxt}"
                    f"/First {child} 0 R /Last {child} 0 R /Count 1 "
                    f"/Dest [{page_num(start)} 0 R /Fit] >>").encode()
        objs[child] = (f"<< /Title (Section {c + 1}.1) /Parent {me} 0 R "
                       f"/Dest [{page_num(min(start + 4, n_pages - 1))} 0 R /Fit] >>").encode()
    objs[outlines] = (f"<< /Type /Outlines /First {chapter_nums[0]} 0 R "
                      f"/Last {chapter_nums[-1]} 0 R /Count {len(chapters)} >>").encode()
    objs[catalog] = f"<< /Type /Catalog /Pages {pages_root} 0 R /Outlines {outlines} 0 R >>".encode()
    info = max(objs) + 1
    objs[info] = b"<< /Title (magic-pdf sample) /Producer (make_sample.py) >>"

    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = {}
    for num in sorted(objs):
        offsets[num] = len(out)
        out += f"{num} 0 obj\n".encode() + objs[num] + b"\nendobj\n"
    size = max(objs) + 1
    xref = len(out)
    out += f"xref\n0 {size}\n".encode() + b"0000000000 65535 f \n"
    for num in range(1, size):
        if num in offsets:
            out += f"{offsets[num]:010d} 00000 n \n".encode()
        else:
            out += b"0000000000 65535 f \n"
    out += f"trailer\n<< /Size {size} /Root {catalog} 0 R /Info {info} 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


if __name__ == "__main__":
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "sample.pdf")
    with open(path, "wb") as f:
        f.write(build())
    print(f"wrote {path}")
