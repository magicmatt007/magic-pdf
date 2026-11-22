#!/usr/bin/env bash
# Downloads a prebuilt PDFium shared library (bblanchon/pdfium-binaries) into third_party/pdfium.
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION="${PDFIUM_VERSION:-latest}"
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  ASSET=pdfium-linux-x64.tgz;   LIB=lib/libpdfium.so ;;
  Linux-aarch64) ASSET=pdfium-linux-arm64.tgz; LIB=lib/libpdfium.so ;;
  Darwin-arm64)  ASSET=pdfium-mac-arm64.tgz;   LIB=lib/libpdfium.dylib ;;
  Darwin-x86_64) ASSET=pdfium-mac-x64.tgz;     LIB=lib/libpdfium.dylib ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)"; exit 1 ;;
esac
if [ "$VERSION" = latest ]; then
  URL="https://github.com/bblanchon/pdfium-binaries/releases/latest/download/$ASSET"
else
  URL="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$VERSION/$ASSET"
fi
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
echo "Downloading $URL"
curl -fL "$URL" -o "$TMP/pdfium.tgz"
tar -xzf "$TMP/pdfium.tgz" -C "$TMP"
mkdir -p third_party/pdfium
cp "$TMP/$LIB" third_party/pdfium/
cp "$TMP/LICENSE" third_party/pdfium/LICENSE 2>/dev/null || true
echo "PDFium installed to third_party/pdfium/$(basename "$LIB")"
