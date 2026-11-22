#!/usr/bin/env bash
# Builds magic-pdf and installs it for the current user (default) or system-wide:
#   packaging/linux/install.sh                 -> ~/.local
#   sudo PREFIX=/usr/local packaging/linux/install.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
PREFIX="${PREFIX:-$HOME/.local}"

if [ ! -f third_party/pdfium/libpdfium.so ]; then
  ./scripts/fetch-pdfium.sh
fi
if [ "${SKIP_BUILD:-0}" != 1 ]; then
  cargo build --release
fi

install -Dm755 target/release/magic-pdf "$PREFIX/bin/magic-pdf"
# The app looks for PDFium in <bin>/../lib/magic-pdf.
install -Dm644 third_party/pdfium/libpdfium.so "$PREFIX/lib/magic-pdf/libpdfium.so"
install -Dm644 third_party/pdfium/LICENSE "$PREFIX/share/licenses/magic-pdf/PDFIUM-LICENSE"
install -Dm644 LICENSE "$PREFIX/share/licenses/magic-pdf/LICENSE"
install -Dm644 packaging/linux/magic-pdf.svg "$PREFIX/share/icons/hicolor/scalable/apps/magic-pdf.svg"
# Use an absolute path: ~/.local/bin is often not on PATH for the desktop session.
sed "s|^Exec=magic-pdf|Exec=$PREFIX/bin/magic-pdf|" packaging/linux/magic-pdf.desktop \
  | install -Dm644 /dev/stdin "$PREFIX/share/applications/magic-pdf.desktop"

update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
echo "Installed magic-pdf to $PREFIX"
