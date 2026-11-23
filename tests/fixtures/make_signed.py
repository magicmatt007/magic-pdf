#!/usr/bin/env python3
"""Generates signed.pdf (RSA) and signed-ec.pdf (ECDSA P-256): one-page PDFs with a
detached CMS signature (adbe.pkcs7.detached) over the whole file, made with a
self-signed test certificate.

Needs the `cryptography` package: python3 tests/fixtures/make_signed.py
"""
import datetime
import os

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa
from cryptography.hazmat.primitives.serialization import pkcs7
from cryptography.x509.oid import NameOID

HERE = os.path.dirname(os.path.abspath(__file__))
SIG_SPACE = 8192  # bytes reserved for the signature (hex: twice that)
RANGE_WIDTH = 10


def certificate(key, name):
    subject = x509.Name([
        x509.NameAttribute(NameOID.COMMON_NAME, name),
        x509.NameAttribute(NameOID.ORGANIZATION_NAME, "magic-pdf tests"),
    ])
    now = datetime.datetime(2026, 1, 1, tzinfo=datetime.timezone.utc)
    return (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(subject)
        .public_key(key.public_key())
        .serial_number(4242)
        .not_valid_before(now)
        .not_valid_after(now + datetime.timedelta(days=3650))
        .sign(key, hashes.SHA256())
    )


def build(key, name):
    placeholder_range = "[0 " + " ".join(["0" * RANGE_WIDTH] * 3) + "]"
    objs = {
        1: b"<< /Type /Catalog /Pages 2 0 R /AcroForm << /Fields [5 0 R] /SigFlags 3 >> >>",
        2: b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        3: b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Contents 4 0 R "
           b"/Resources << /Font << /F1 7 0 R >> >> /Annots [5 0 R] >>",
        5: b"<< /Type /Annot /Subtype /Widget /FT /Sig /T (Signature1) /Rect [0 0 0 0] /F 132 "
           b"/P 3 0 R /V 6 0 R >>",
        6: ("<< /Type /Sig /Filter /Adobe.PPKLite /SubFilter /adbe.pkcs7.detached "
            f"/ByteRange {placeholder_range} /Contents <{'0' * (SIG_SPACE * 2)}> "
            "/M (D:20261123120000+01'00') /Reason (Approved) >>").encode(),
        7: b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    }
    text = b"BT /F1 24 Tf 72 700 Td (A signed document) Tj ET"
    objs[4] = b"<< /Length %d >>\nstream\n" % len(text) + text + b"\nendstream"

    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = {}
    for num in sorted(objs):
        offsets[num] = len(out)
        out += b"%d 0 obj\n" % num + objs[num] + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for num in sorted(objs):
        out += b"%010d 00000 n \n" % offsets[num]
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)

    # Byte range: everything except the <...> of /Contents.
    start = out.index(b"/Contents <") + len(b"/Contents ")
    end = out.index(b">", start) + 1
    ranges = [0, start, end, len(out) - end]
    filled = "[" + " ".join(str(v).rjust(RANGE_WIDTH if i else 1) for i, v in enumerate(ranges)) + "]"
    filled = filled.ljust(len(placeholder_range))
    at = out.index(placeholder_range.encode())
    out[at:at + len(placeholder_range)] = filled.encode()

    data = bytes(out[:start]) + bytes(out[end:])
    cert = certificate(key, name)
    sig = (
        pkcs7.PKCS7SignatureBuilder()
        .set_data(data)
        .add_signer(cert, key, hashes.SHA256())
        .sign(serialization.Encoding.DER, [pkcs7.PKCS7Options.DetachedSignature, pkcs7.PKCS7Options.Binary])
    )
    assert len(sig) <= SIG_SPACE, "signature too large"
    hexsig = sig.hex().encode().ljust(SIG_SPACE * 2, b"0")
    out[start + 1:end - 1] = hexsig
    return bytes(out)


if __name__ == "__main__":
    for name, key in [
        ("signed.pdf", rsa.generate_private_key(public_exponent=65537, key_size=2048)),
        ("signed-ec.pdf", ec.generate_private_key(ec.SECP256R1())),
    ]:
        with open(os.path.join(HERE, name), "wb") as f:
            f.write(build(key, "magic-pdf Test Signer"))
        print("wrote", name)
