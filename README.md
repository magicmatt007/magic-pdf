# magic-pdf

A lightweight, cross-platform PDF viewer (Linux, Windows, macOS) built for wide
monitors: its default **horizontal layout** puts every page side by side, fitted to the
window height, and the mouse wheel scrolls left and right through the document.

Built with Rust, [egui](https://github.com/emilk/egui) and Google's
[PDFium](https://pdfium.googlesource.com/pdfium/) (via `pdfium-render`).

## Features

- Layouts: horizontal, vertical, single page, facing (cover + pairs), grid overview
  (double-click a page to open it in the previous layout); per-file memory
- Fit width / height / page, zoom presets, Ctrl+wheel zoom around the cursor, rotation;
  sharp tiles at high zoom
- Thumbnails, outline/bookmarks, attachments
- Search with highlighted matches and the words around each hit, in one file or in all
  open tabs at once
- Text selection and copy, internal and external links
- Tabs, recent files, drag & drop, password-protected files, night mode
- English and German interface

- Annotations: highlight, underline, strikeout, sticky notes, rich text boxes typed
  directly on the page (bold, italic, underline, strikethrough, colours and sizes per
  word, alignment; Ctrl+B/I/U; stored as standard PDF rich text), freehand drawing,
  rectangles and ellipses (with separate fill colours), lines and arrows (Shift: 45° steps), polygons and clouds, stamps
  (built-in, own texts with name and date, pictures) and signatures (drawn or from a
  picture); any colour (70-colour palette, recent colours, custom colour or
  hex code), line widths; copy and paste annotations (also between tabs)
- Comments: a Comments sidebar with search and filters by type, author and status,
  reply threads and review status (Accepted, Rejected, …)
- Measuring: distances and areas with a drawing scale (1:n; mm, cm, m, km, in, ft),
  shown live and kept on the page
- Annotation editing: drag to move, resize with handles (Shift keeps proportions),
  nudge with arrow keys; undo/redo, Save / Save As (deleted annotations are removed from the file)

- Form filling: text fields, checkboxes, radio buttons, combo and list boxes
  (typing, Tab between fields, copy/paste, field undo); filled values are saved
- Printing: a print dialog backed by CUPS on Linux/macOS (printer, copies, page
  range, two-sided, fit to paper) and the native print dialog on Windows

- Page editing in the thumbnail sidebar or the Pages menu: select pages (Ctrl/Shift
  click), drag to reorder, rotate, delete, insert blank pages or pages from other
  PDFs (also by dropping files between thumbnails), extract to a new file, split
  into several files; crop pages, headers and footers with page numbers, date and file
  name; add, rename, move and indent bookmarks; all undoable and saved with the document
- OCR: Pages → Recognise text makes scanned pages searchable and selectable with
  [Tesseract](https://github.com/tesseract-ocr/tesseract) (installed separately; the
  dialog says how), as an invisible text layer over the page image
- Compare two PDFs (File → Compare with): side by side, scrolling and zooming together,
  changed words marked red and green, a bar to step through pages with changes
- E-invoices (ZUGFeRD / Factur-X) shown and checked, document properties (Ctrl+D) and
  File → Save optimized copy (lossless compression, pictures scaled down to a chosen dpi);
  see [E-invoices](#e-invoices)

- One window, many tabs: PDFs opened from the desktop join the running window;
  drag a tab out to give it its own window, or onto another magic-pdf window to
  move it there (unsaved changes included; on Wayland use the tab's right-click
  menu, as Wayland does not report window positions)
- Update notices: checks GitHub at most once a day (Help menu to turn off); on
  Windows, macOS and in the Linux AppImage, *Install* downloads the new version,
  verifies its SHA-256 checksum and updates in place (macOS: opens the disk image)

Document JavaScript is never executed.

## Installing

**Windows:** download `magic-pdf-<version>-setup.exe` from the
[latest release](https://github.com/magicmatt007/magic-pdf/releases/latest) and run it.
No administrator rights are needed. It adds a Start menu entry and registers magic-pdf
for PDF files; tick the last option at the end to open Windows Settings and make it your
default PDF viewer (also available later under Help). The installer is not code-signed,
so Windows SmartScreen may ask for confirmation: *More info* → *Run anyway*. A portable
zip is available too.

**macOS** (11 or later, Apple Silicon and Intel): download `magic-pdf-<version>-macos.dmg`
from the same release, open it and drag magic-pdf into Applications. The app is not
notarised by Apple, so macOS blocks the first start: click *Done*, then open *System
Settings → Privacy & Security* and click *Open Anyway* next to magic-pdf (once). To make it
your PDF viewer, select a PDF in Finder, *File → Get Info*, pick magic-pdf under *Open
with* and click *Change All…*. To build the app yourself on a Mac:
`packaging/macos/build-app.sh` (writes `dist/magic-pdf.app` and the `.dmg`).

**Linux** (x86_64, glibc 2.35 or newer, e.g. Ubuntu 22.04, Debian 12, Fedora 36 and
later): download `magic-pdf-<version>-x86_64.AppImage` from the same release, make it
executable and run it:

```bash
chmod +x magic-pdf-*-x86_64.AppImage
./magic-pdf-*-x86_64.AppImage
```

Keep it somewhere permanent (for example `~/Applications`), since updates from within
the app replace that file. If it fails with a FUSE error, install `libfuse2` (Ubuntu
24.04: `libfuse2t64`) or run it with `APPIMAGE_EXTRACT_AND_RUN=1`. For an application
menu entry and "Open with" for PDF files, let a tool such as Gear Lever or
AppImageLauncher integrate it, or build from source and install for your user (see
below). To build the AppImage yourself: `packaging/linux/build-appimage.sh` (writes
`dist/magic-pdf-<version>-x86_64.AppImage`).

## Building

```bash
./scripts/fetch-pdfium.sh          # Linux/macOS: downloads libpdfium into third_party/pdfium
cargo run --release -- file.pdf
```

On Windows run `scripts\fetch-pdfium.ps1` instead. PDFium is loaded at runtime from (in
order) `$MAGIC_PDF_PDFIUM_DIR`, the executable's directory, `third_party/pdfium` and the
system library path, so a distribution is just the executable plus one shared library.

## Installing on Linux from source

```bash
packaging/linux/install.sh      # builds and installs into ~/.local
packaging/linux/uninstall.sh    # removes it again
```

This adds magic-pdf to the application menu and to "Open with" for PDF files. Builds
from source are updated with `git pull` and the install script again (the update notice
links to the release page). Use
`sudo PREFIX=/usr/local packaging/linux/install.sh` for a system-wide install.

## Releasing

1. Set the new `version` in `Cargo.toml` and commit.
2. Tag and push: `git tag v0.2.0 && git push origin v0.2.0`.

The *Release* workflow then builds the Windows installer, the portable zip, the macOS
disk image and the Linux AppImage and publishes them as a GitHub release. Running the workflow by hand (Actions → Release → Run
workflow) builds the same files as a downloadable artifact without publishing anything.
CI runs clippy and the tests on Linux, Windows and macOS for every push.

## E-invoices

PDFs with ZUGFeRD / Factur-X invoice data (an embedded `factur-x.xml` or similar) get a
bar above the page and an **E-invoice** window: seller and buyer, items, VAT, totals and
payment details, with checks that the amounts add up, that the total appears on the page,
and that fields EN 16931 requires are present. Attached files are listed in the sidebar's
**Attachments** tab (open or save them). **File → Document properties** (Ctrl+D) shows
and edits title, author, subject and keywords (also in the XMP metadata).

## Languages

The interface is in English and German (Edit → Settings → Language; "System" follows
the computer's language). Texts in the code are English and go through `t("…")` /
`tf("… {name} …", …)` in `src/i18n.rs`; the German texts are in `src/i18n/de.rs`, and a
test fails when a text has no translation.

## Debugging

`MAGIC_PDF_SCREENSHOT=out.ppm magic-pdf file.pdf` opens the file, saves a screenshot
of the app's own window after three seconds and quits (`MAGIC_PDF_TOOL=highlight`, `note`, … picks
the starting tool). `MAGIC_PDF_SELFTEST=out.txt magic-pdf a.pdf b.pdf` writes each opened
file's page count to `out.txt` and quits; the release workflow uses it to smoke-test the
macOS app bundle. `MAGIC_PDF_LANG=de|en` overrides the language setting.

## Tests

```bash
python3 tests/fixtures/make_sample.py   # regenerates tests/fixtures/sample.pdf
python3 tests/fixtures/make_form.py     # regenerates tests/fixtures/form.pdf
python3 tests/fixtures/make_scanned.py  # regenerates tests/fixtures/scanned.pdf
cargo test                               # unit tests + PDFium engine tests
```

The engine tests need PDFium (`scripts/fetch-pdfium.sh`); the OCR test is skipped when
Tesseract with English is not installed.

## Shortcuts

| Keys | Action |
| --- | --- |
| Wheel / Shift+Wheel | Scroll along / across the layout |
| Ctrl+Wheel, Ctrl+±, Ctrl+0 | Zoom, actual size |
| PgUp/PgDn, Home/End, Space | Page navigation |
| Ctrl+1…5 | Horizontal, vertical, single, facing, grid |
| Double-click a page in the grid | Open it in the previous layout |
| Ctrl+R / Ctrl+Shift+R | Rotate |
| H / S | Hand / text-select tool |
| U / N / T / D / R / E | Highlight, note, text box, draw, rectangle, ellipse |
| L / A / P / C | Line, arrow, polygon, cloud (Enter or double-click finishes) |
| K / G / M | Stamp, signature, measure distance |
| Ctrl+C / Ctrl+V on an annotation | Copy it, paste it under the mouse |
| Esc | Cancel what is in progress and return to the Hand tool |
| Ctrl+Z / Ctrl+Y, Del | Undo / redo, delete selected annotation |
| Ctrl+S / Ctrl+Shift+S | Save / save as |
| Ctrl+P | Print |
| Click / Ctrl / Shift + click thumbnail | Select pages (Del deletes them) |
| Ctrl+F, Enter, F3 | Search |
| Ctrl+D | Document properties |
| Ctrl+, | Settings |
| F9 | Sidebar |

## License

Copyright (C) 2026 The magic-pdf contributors

This program is free software: you can redistribute it and/or modify it under the terms
of the GNU Affero General Public License as published by the Free Software Foundation,
either version 3 of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A
PARTICULAR PURPOSE. See the [GNU Affero General Public License](LICENSE) for details.

Toolbar icons are from [Phosphor Icons](https://phosphoricons.com) (MIT license,
see `assets/fonts/phosphor/LICENSE`). Text boxes use
[Noto Sans](https://notofonts.github.io) (SIL Open Font License, see
`assets/fonts/noto-sans/OFL.txt`).

PDFium is distributed under its own BSD-style license; `scripts/fetch-pdfium.*` places a
copy in `third_party/pdfium/LICENSE`, which must ship alongside the library.
