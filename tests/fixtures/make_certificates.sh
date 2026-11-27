#!/bin/sh
# Generates the test signing certificates (password "magic"): signer-rsa.p12 (RSA 2048,
# modern AES encryption), signer-ec.p12 (ECDSA P-256) and signer-legacy.p12 (RSA with the
# older 3DES encryption that Windows exports often use). Needs openssl 3.
set -e
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
subj() { echo "/CN=$1/O=magic-pdf tests"; }

openssl req -x509 -newkey rsa:2048 -nodes -keyout "$tmp/rsa.key" -out "$tmp/rsa.crt" \
    -days 3650 -subj "$(subj 'Test Signer RSA')" 2>/dev/null
openssl pkcs12 -export -inkey "$tmp/rsa.key" -in "$tmp/rsa.crt" -name "Test Signer RSA" \
    -passout pass:magic -out signer-rsa.p12

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout "$tmp/ec.key" \
    -out "$tmp/ec.crt" -days 3650 -subj "$(subj 'Test Signer EC')" 2>/dev/null
openssl pkcs12 -export -inkey "$tmp/ec.key" -in "$tmp/ec.crt" -name "Test Signer EC" \
    -passout pass:magic -out signer-ec.p12

openssl pkcs12 -export -inkey "$tmp/rsa.key" -in "$tmp/rsa.crt" -name "Test Signer RSA" \
    -passout pass:magic -legacy -out signer-legacy.p12
