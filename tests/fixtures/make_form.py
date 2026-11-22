#!/usr/bin/env python3
"""Generates form.pdf: one page with an AcroForm text field and a checkbox.

Pure standard library: python3 tests/fixtures/make_form.py
"""
import os


def stream(content: bytes, extra: str = "") -> bytes:
    return b"<< /Length %d %s>>\nstream\n" % (len(content), extra.encode()) + content + b"\nendstream"


def build():
    objs = {}
    objs[1] = b"<< /Type /Catalog /Pages 2 0 R /AcroForm 8 0 R >>"
    objs[2] = b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>"
    objs[3] = (b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] "
               b"/Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R /Annots [6 0 R 7 0 R] >>")
    objs[4] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"
    objs[5] = stream(b"BT /F1 24 Tf 100 740 Td (Form test) Tj ET\n"
                     b"BT /F1 14 Tf 100 690 Td (Name:) Tj ET\n"
                     b"BT /F1 14 Tf 130 605 Td (I agree) Tj ET")
    # Text field.
    objs[6] = (b"<< /Type /Annot /Subtype /Widget /FT /Tx /T (name) /V () /F 4 "
               b"/Rect [100 650 400 680] /DA (/Helv 14 Tf 0 g) /MK << /BC [0 0 0] >> /P 3 0 R >>")
    # Checkbox with explicit on/off appearances.
    objs[7] = (b"<< /Type /Annot /Subtype /Widget /FT /Btn /T (agree) /V /Off /AS /Off /F 4 "
               b"/Rect [100 600 120 620] /MK << /CA (4) /BC [0 0 0] >> /DA (/ZaDb 0 Tf 0 g) "
               b"/AP << /N << /Yes 9 0 R /Off 10 0 R >> >> /P 3 0 R >>")
    objs[8] = (b"<< /Fields [6 0 R 7 0 R] /NeedAppearances true /DA (/Helv 0 Tf 0 g) "
               b"/DR << /Font << /Helv 11 0 R /ZaDb 12 0 R >> >> >>")
    box = b"0 G 1 w 0.5 0.5 19 19 re S"
    objs[9] = stream(box + b"\nBT /ZaDb 16 Tf 3 4 Td (4) Tj ET",
                     "/Type /XObject /Subtype /Form /BBox [0 0 20 20] /Resources << /Font << /ZaDb 12 0 R >> >> ")
    objs[10] = stream(box, "/Type /XObject /Subtype /Form /BBox [0 0 20 20] ")
    objs[11] = b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
    objs[12] = b"<< /Type /Font /Subtype /Type1 /BaseFont /ZapfDingbats >>"

    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = {}
    for num in sorted(objs):
        offsets[num] = len(out)
        out += f"{num} 0 obj\n".encode() + objs[num] + b"\nendobj\n"
    size = max(objs) + 1
    xref = len(out)
    out += f"xref\n0 {size}\n".encode() + b"0000000000 65535 f \n"
    for num in range(1, size):
        out += f"{offsets[num]:010d} 00000 n \n".encode()
    out += f"trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
    return bytes(out)


if __name__ == "__main__":
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "form.pdf")
    with open(path, "wb") as f:
        f.write(build())
    print(f"wrote {path}")
