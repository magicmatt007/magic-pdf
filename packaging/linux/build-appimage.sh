#!/usr/bin/env bash
# Builds a portable Linux AppImage (x86_64):
#   packaging/linux/build-appimage.sh      -> dist/magic-pdf-<version>-x86_64.AppImage
# Bundles the release binary, libpdfium.so (scripts/fetch-pdfium.sh, honours
# PDFIUM_VERSION), the desktop entry, icons and licences. System libraries (glibc,
# OpenGL, X11/Wayland) come from the host, so build on an old distribution (CI uses
# Ubuntu 22.04) for the widest compatibility. Set SKIP_BUILD=1 to reuse
# target/release/magic-pdf. Without FUSE, run with APPIMAGE_EXTRACT_AND_RUN=1.
set -euo pipefail
cd "$(dirname "$0")/../.."
VERSION="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
ARCH=x86_64
APPIMAGETOOL_VERSION="${APPIMAGETOOL_VERSION:-1.9.1}"
DIST=dist
APPDIR="$DIST/magic-pdf.AppDir"
OUT="$DIST/magic-pdf-$VERSION-$ARCH.AppImage"

[ "$(uname -m)" = "$ARCH" ] || { echo "build-appimage.sh builds $ARCH AppImages only"; exit 1; }

echo "Building magic-pdf $VERSION"
# Always fetch when a version is pinned, so the bundle matches PDFIUM_VERSION.
if [ -n "${PDFIUM_VERSION:-}" ] || [ ! -f third_party/pdfium/libpdfium.so ]; then
  ./scripts/fetch-pdfium.sh
fi
if [ "${SKIP_BUILD:-0}" != 1 ]; then
  cargo build --release
fi

rm -rf "$APPDIR"
LIC="$APPDIR/usr/share/licenses/magic-pdf"
install -Dm755 target/release/magic-pdf "$APPDIR/usr/bin/magic-pdf"
# The app looks for PDFium in <bin>/../lib/magic-pdf (src/engine/mod.rs).
install -Dm644 third_party/pdfium/libpdfium.so "$APPDIR/usr/lib/magic-pdf/libpdfium.so"
install -Dm644 LICENSE "$LIC/LICENSE"
install -Dm644 third_party/pdfium/LICENSE "$LIC/PDFIUM-LICENSE"
install -Dm644 assets/fonts/phosphor/LICENSE "$LIC/PHOSPHOR-LICENSE"
install -Dm644 assets/fonts/noto-sans/OFL.txt "$LIC/NOTO-SANS-LICENSE"
install -Dm644 packaging/linux/magic-pdf.desktop "$APPDIR/usr/share/applications/magic-pdf.desktop"
install -Dm644 packaging/linux/magic-pdf.svg "$APPDIR/usr/share/icons/hicolor/scalable/apps/magic-pdf.svg"
install -Dm644 assets/icons/magic-pdf-256.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/magic-pdf.png"

# AppImage root: entry point, desktop entry and icon.
ln -s usr/bin/magic-pdf "$APPDIR/AppRun"
ln -s usr/share/applications/magic-pdf.desktop "$APPDIR/magic-pdf.desktop"
ln -s usr/share/icons/hicolor/scalable/apps/magic-pdf.svg "$APPDIR/magic-pdf.svg"
ln -s usr/share/icons/hicolor/256x256/apps/magic-pdf.png "$APPDIR/.DirIcon"
if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$APPDIR/magic-pdf.desktop"
fi

# appimagetool (cached in target/ between runs).
TOOL="target/appimagetool-$APPIMAGETOOL_VERSION-$ARCH.AppImage"
if [ ! -x "$TOOL" ]; then
  curl -fsSL -o "$TOOL.part" \
    "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$ARCH.AppImage"
  chmod +x "$TOOL.part"
  mv "$TOOL.part" "$TOOL"
fi
rm -f "$OUT"
ARCH=$ARCH "$TOOL" --no-appstream "$APPDIR" "$OUT"
echo "Built $OUT"
