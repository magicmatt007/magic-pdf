#!/usr/bin/env bash
# Builds magic-pdf.app (universal: Apple Silicon + Intel) and a .dmg on macOS:
#   packaging/macos/build-app.sh            -> dist/magic-pdf.app, dist/magic-pdf-<version>-macos.dmg
# The app is signed ad hoc (no Apple Developer ID): on first launch macOS asks to
# allow it once (System Settings > Privacy & Security > Open Anyway).
set -euo pipefail
cd "$(dirname "$0")/../.."
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
PDFIUM_VERSION="${PDFIUM_VERSION:-}"
DIST=dist
APP="$DIST/magic-pdf.app"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "Building magic-pdf $VERSION"
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

# PDFium for both architectures, combined into one universal library.
for arch in arm64 x64; do
  if [ -n "$PDFIUM_VERSION" ]; then
    url="https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$PDFIUM_VERSION/pdfium-mac-$arch.tgz"
  else
    url="https://github.com/bblanchon/pdfium-binaries/releases/latest/download/pdfium-mac-$arch.tgz"
  fi
  mkdir -p "$TMP/$arch"
  curl -fsSL "$url" -o "$TMP/$arch.tgz"
  tar -xzf "$TMP/$arch.tgz" -C "$TMP/$arch"
done

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks" "$APP/Contents/Resources/licenses"
lipo -create -output "$APP/Contents/MacOS/magic-pdf" \
  target/aarch64-apple-darwin/release/magic-pdf target/x86_64-apple-darwin/release/magic-pdf
lipo -create -output "$APP/Contents/Frameworks/libpdfium.dylib" \
  "$TMP/arm64/lib/libpdfium.dylib" "$TMP/x64/lib/libpdfium.dylib"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"
cp assets/icons/magic-pdf.icns "$APP/Contents/Resources/"
cp LICENSE "$APP/Contents/Resources/licenses/LICENSE.txt"
cp "$TMP/arm64/LICENSE" "$APP/Contents/Resources/licenses/PDFIUM-LICENSE.txt"
cp assets/fonts/phosphor/LICENSE "$APP/Contents/Resources/licenses/PHOSPHOR-LICENSE.txt"
cp assets/fonts/noto-sans/OFL.txt "$APP/Contents/Resources/licenses/NOTO-SANS-LICENSE.txt"
plutil -lint "$APP/Contents/Info.plist"

# Ad-hoc signature (required on Apple Silicon; no Developer ID needed).
codesign --force --sign - "$APP/Contents/Frameworks/libpdfium.dylib"
codesign --force --sign - "$APP"
codesign --verify --verbose "$APP"
lipo -info "$APP/Contents/MacOS/magic-pdf"

# Disk image with a shortcut to Applications for drag-and-drop installation.
mkdir -p "$TMP/dmg"
cp -R "$APP" "$TMP/dmg/"
ln -s /Applications "$TMP/dmg/Applications"
DMG="$DIST/magic-pdf-$VERSION-macos.dmg"
rm -f "$DMG"
hdiutil create -volname "magic-pdf $VERSION" -srcfolder "$TMP/dmg" -ov -format UDZO "$DMG"
echo "Built $APP and $DMG"
