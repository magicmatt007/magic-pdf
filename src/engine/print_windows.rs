//! Native printing on Windows: the system print dialog, then each page is rendered by
//! PDFium at printer resolution and sent to the printer with GDI.

use crate::i18n::{t, tf};
use std::ptr;

use windows_sys::Win32::Foundation::GlobalFree;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteDC, GetDeviceCaps, HORZRES, SRCCOPY, StretchDIBits,
    VERTRES,
};
use windows_sys::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};
use windows_sys::Win32::UI::Controls::Dialogs::{
    PD_CURRENTPAGE, PD_NOSELECTION, PD_PAGENUMS, PD_RETURNDC, PD_USEDEVMODECOPIESANDCOLLATE, PRINTDLGW, PrintDlgW,
};

use super::pdf::Doc;

/// Shows the print dialog and prints. Returns the number of pages printed, or `None`
/// if the user cancelled.
pub fn print(doc: &Doc, title: &str, current_page: usize) -> Result<Option<usize>, String> {
    let count = doc.page_count();
    if count == 0 {
        return Ok(Some(0));
    }
    // SAFETY: PRINTDLGW is a plain C struct; zero is valid for all optional fields.
    let mut pd: PRINTDLGW = unsafe { std::mem::zeroed() };
    pd.lStructSize = std::mem::size_of::<PRINTDLGW>() as u32;
    pd.Flags = PD_RETURNDC | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE;
    pd.nMinPage = 1;
    pd.nMaxPage = count.min(u16::MAX as usize) as u16;
    pd.nFromPage = 1;
    pd.nToPage = pd.nMaxPage;
    pd.nCopies = 1;

    if unsafe { PrintDlgW(&mut pd) } == 0 {
        return Ok(None);
    }
    let hdc = pd.hDC;
    let result = (|| {
        if hdc.is_null() {
            return Err(t("the printer could not be opened").to_string());
        }
        let pages: Vec<usize> = if pd.Flags & PD_PAGENUMS != 0 {
            let from = (pd.nFromPage as usize).max(1) - 1;
            let to = (pd.nToPage as usize).min(count);
            (from..to).collect()
        } else if pd.Flags & PD_CURRENTPAGE != 0 {
            vec![current_page.min(count - 1)]
        } else {
            (0..count).collect()
        };
        print_pages(doc, hdc, title, &pages)?;
        Ok(Some(pages.len()))
    })();
    unsafe {
        if !hdc.is_null() {
            DeleteDC(hdc);
        }
        if !pd.hDevMode.is_null() {
            GlobalFree(pd.hDevMode);
        }
        if !pd.hDevNames.is_null() {
            GlobalFree(pd.hDevNames);
        }
    }
    result
}

fn print_pages(
    doc: &Doc,
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    title: &str,
    pages: &[usize],
) -> Result<(), String> {
    let name: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let info = DOCINFOW {
        cbSize: std::mem::size_of::<DOCINFOW>() as i32,
        lpszDocName: name.as_ptr(),
        lpszOutput: ptr::null(),
        lpszDatatype: ptr::null(),
        fwType: 0,
    };
    unsafe {
        if StartDocW(hdc, &info) <= 0 {
            return Err(t("the print job could not be started").into());
        }
        // Printable area in device pixels.
        let area_w = GetDeviceCaps(hdc, HORZRES as i32).max(1) as f32;
        let area_h = GetDeviceCaps(hdc, VERTRES as i32).max(1) as f32;
        for &index in pages {
            let (pw, ph) = doc.page_size(index);
            // Fit the page into the printable area, keeping its aspect ratio.
            let scale = (area_w / pw).min(area_h / ph);
            let w = (pw * scale).round().max(1.0) as usize;
            let h = (ph * scale).round().max(1.0) as usize;
            let Some(page) = doc.page(index) else { continue };
            let Some(bgra) = page.render_for_print(w, h, false) else {
                AbortDoc(hdc);
                return Err(tf("page {page} could not be rendered", &[("page", &(index + 1))]));
            };
            if StartPage(hdc) <= 0 {
                AbortDoc(hdc);
                return Err(t("the printer rejected a page").into());
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let x = ((area_w - w as f32) / 2.0) as i32;
            let y = ((area_h - h as f32) / 2.0) as i32;
            StretchDIBits(
                hdc,
                x,
                y,
                w as i32,
                h as i32,
                0,
                0,
                w as i32,
                h as i32,
                bgra.as_ptr().cast(),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            if EndPage(hdc) <= 0 {
                AbortDoc(hdc);
                return Err(t("the printer rejected a page").into());
            }
        }
        if EndDoc(hdc) <= 0 {
            return Err(t("the print job could not be finished").into());
        }
    }
    Ok(())
}
