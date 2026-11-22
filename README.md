# magic-pdf

A lightweight, cross-platform PDF viewer (Linux, Windows, later macOS) built for wide
monitors: its default **horizontal layout** puts every page side by side, fitted to the
window height, and the mouse wheel scrolls left and right through the document.

Built with Rust, [egui](https://github.com/emilk/egui) and Google's
[PDFium](https://pdfium.googlesource.com/pdfium/) (via `pdfium-render`).

## Features

- Layouts: horizontal, vertical, single page, facing (cover + pairs), grid; per-file memory
- Fit width / height / page, zoom presets, Ctrl+wheel zoom around the cursor, rotation
- Thumbnails, outline/bookmarks, document search with highlighted matches
- Text selection and copy, internal and external links
- Tabs, recent files, drag & drop, password-protected files, night mode

- Annotations: highlight, underline, strikeout, sticky notes, freehand drawing,
  rectangles and ellipses; colours, line widths, comments, a Comments sidebar,
  undo/redo, Save / Save As (deleted annotations are removed from the file)

- Form filling: text fields, checkboxes, radio buttons, combo and list boxes
  (typing, Tab between fields, copy/paste, field undo); filled values are saved
- Printing: a print dialog backed by CUPS on Linux/macOS (printer, copies, page
  range, two-sided, fit to paper) and the native print dialog on Windows

Page editing is planned for a later phase. Document JavaScript is never executed.

## Building

```bash
./scripts/fetch-pdfium.sh          # Linux/macOS: downloads libpdfium into third_party/pdfium
cargo run --release -- file.pdf
```

On Windows run `scripts\fetch-pdfium.ps1` instead. PDFium is loaded at runtime from (in
order) `$MAGIC_PDF_PDFIUM_DIR`, the executable's directory, `third_party/pdfium` and the
system library path, so a distribution is just the executable plus one shared library.

## Installing on Linux

```bash
packaging/linux/install.sh      # builds and installs into ~/.local
packaging/linux/uninstall.sh    # removes it again
```

This adds magic-pdf to the application menu and to "Open with" for PDF files. Use
`sudo PREFIX=/usr/local packaging/linux/install.sh` for a system-wide install.

## Debugging

`MAGIC_PDF_SCREENSHOT=out.ppm magic-pdf file.pdf` opens the file, saves a screenshot
of the app's own window after three seconds and quits.

## Tests

```bash
python3 tests/fixtures/make_sample.py   # regenerates tests/fixtures/sample.pdf
python3 tests/fixtures/make_form.py     # regenerates tests/fixtures/form.pdf
cargo test                               # layout/viewport unit tests + PDFium engine test
```

## Shortcuts

| Keys | Action |
| --- | --- |
| Wheel / Shift+Wheel | Scroll along / across the layout |
| Ctrl+Wheel, Ctrl+±, Ctrl+0 | Zoom, actual size |
| PgUp/PgDn, Home/End, Space | Page navigation |
| Ctrl+1…5 | Horizontal, vertical, single, facing, grid |
| Ctrl+R / Ctrl+Shift+R | Rotate |
| H / S | Hand / text-select tool |
| U / N / D / R / E | Highlight, note, draw, rectangle, ellipse |
| Ctrl+Z / Ctrl+Y, Del | Undo / redo, delete selected annotation |
| Ctrl+S / Ctrl+Shift+S | Save / save as |
| Ctrl+P | Print |
| Ctrl+F, Enter, F3 | Search |
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
see `assets/fonts/phosphor/LICENSE`).

PDFium is distributed under its own BSD-style license; `scripts/fetch-pdfium.*` places a
copy in `third_party/pdfium/LICENSE`, which must ship alongside the library.
