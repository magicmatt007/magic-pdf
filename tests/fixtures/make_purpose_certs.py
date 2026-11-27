#!/usr/bin/env python3
"""Generates certificates (DER) for the certificate list filter: cert-login.der (for
logging in only, like a Windows device certificate), cert-email.der (signing and e-mail
protection) and cert-expired.der (expired in 2021); and a chain for the trust check:
chain-root.der (an RSA root), chain-inter.der (an ECDSA intermediate it issued) and
chain-leaf.der (an ECDSA signing certificate the intermediate issued, 2026-2036).

Needs the `cryptography` package: python3 tests/fixtures/make_purpose_certs.py
"""
import datetime
import os

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, rsa
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


def issued(name, key, issuer_name, issuer_key, start, end, ca):
    b = (
        x509.CertificateBuilder()
        .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, name)]))
        .issuer_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, issuer_name)]))
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(start)
        .not_valid_after(end)
        .add_extension(x509.BasicConstraints(ca=ca, path_length=None), critical=True)
    )
    if ca:
        b = b.add_extension(x509.KeyUsage(False, False, False, False, False, True, True, False, False), critical=True)
    else:
        b = b.add_extension(x509.KeyUsage(*usage(True)), critical=True)
        b = b.add_extension(x509.ExtendedKeyUsage([ExtendedKeyUsageOID.EMAIL_PROTECTION]), critical=False)
    return b.sign(issuer_key, hashes.SHA256()).public_bytes(serialization.Encoding.DER)


root_key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
inter_key = ec.generate_private_key(ec.SECP256R1())
leaf_key = ec.generate_private_key(ec.SECP256R1())
chain = {
    "chain-root.der": issued("Test Root CA", root_key, "Test Root CA", root_key, now, now + datetime.timedelta(days=7300), True),
    "chain-inter.der": issued("Test Issuing CA", inter_key, "Test Root CA", root_key, now, now + datetime.timedelta(days=5000), True),
    "chain-leaf.der": issued("Anna Signer", leaf_key, "Test Issuing CA", inter_key, now, later, False),
}
for name, der in chain.items():
    with open(os.path.join(HERE, name), "wb") as f:
        f.write(der)
