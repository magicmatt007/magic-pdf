#!/bin/sh
# Generates signed-timestamped.pdf: sample.pdf signed with signer-rsa.p12 and
# timestamped by a throwaway timestamp authority made with openssl (openssl 3).
set -e
cd "$(dirname "$0")/../.."
cargo test --bin magic-pdf make_timestamped_fixture -- --ignored
