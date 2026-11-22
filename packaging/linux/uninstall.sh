#!/usr/bin/env bash
# Removes what install.sh installed. Use the same PREFIX as for installing.
set -euo pipefail
PREFIX="${PREFIX:-$HOME/.local}"
rm -f "$PREFIX/bin/magic-pdf" \
      "$PREFIX/share/applications/magic-pdf.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/magic-pdf.svg"
rm -rf "$PREFIX/lib/magic-pdf" "$PREFIX/share/licenses/magic-pdf"
update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
gtk-update-icon-cache -q -t "$PREFIX/share/icons/hicolor" 2>/dev/null || true
echo "Removed magic-pdf from $PREFIX (settings in ~/.config/magic-pdf are kept)"
