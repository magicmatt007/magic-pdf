#!/usr/bin/env python3
"""Generates certificates (DER) for the certificate list filter: cert-login.der (for
logging in only, like a Windows device certificate), cert-email.der (signing and e-mail
protection) and cert-expired.der (expired in 2021).

Needs the `cryptography` package: python3 tests/fixtures/make_purpose_certs.py
"""
import datetime
import os

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import ExtendedKeyUsageOID, NameOID

HERE = os.path.dirname(os.path.abspath(__file__))
UTC = datetime.timezone.utc


def make(name, start, end, usage, purposes):
    key = ec.generate_private_key(ec.SECP256R1())
    subject = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, name)])
    b = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(subject)
        .public_key(key.public_key())
        .serial_number(7)
        .not_valid_before(start)
        .not_valid_after(end)
        .add_extension(x509.KeyUsage(*usage), critical=True)
        .add_extension(x509.ExtendedKeyUsage(purposes), critical=False)
    )
    return b.sign(key, hashes.SHA256()).public_bytes(serialization.Encoding.DER)


def usage(signs):
    # digital_signature, content_commitment, key_encipherment, data_encipherment,
    # key_agreement, key_cert_sign, crl_sign, encipher_only, decipher_only
    return (signs, signs, not signs, False, False, False, False, False, False)


now = datetime.datetime(2026, 1, 1, tzinfo=UTC)
later = now + datetime.timedelta(days=3650)
files = {
    "cert-login.der": make("Device login", now, later, usage(False), [ExtendedKeyUsageOID.CLIENT_AUTH]),
    "cert-email.der": make(
        "Anna Muster", now, later, usage(True), [ExtendedKeyUsageOID.EMAIL_PROTECTION, ExtendedKeyUsageOID.CLIENT_AUTH]
    ),
    "cert-expired.der": make(
        "Old Signer",
        datetime.datetime(2019, 1, 1, tzinfo=UTC),
        datetime.datetime(2021, 1, 1, tzinfo=UTC),
        usage(True),
        [ExtendedKeyUsageOID.EMAIL_PROTECTION],
    ),
}
for name, der in files.items():
    with open(os.path.join(HERE, name), "wb") as f:
        f.write(der)
