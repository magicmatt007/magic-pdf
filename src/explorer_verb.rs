//! Windows: Explorer's "Open" for any number of selected PDFs.
//!
//! A plain command line ("magic-pdf.exe" "%1") makes Explorer start one process per
//! file, and Explorer refuses selections over 15 files. Instead, the installer
//! registers the verb with `DelegateExecute` (this COM class) and
//! `MultiSelectModel=Player`, so Explorer starts `magic-pdf.exe -Embedding` once and
//! hands it the whole selection (`IObjectWithSelection::SetSelection`, then
//! `IExecuteCommand::Execute`). Modelled on Microsoft's ExecuteCommandVerb sample
//! (Windows-classic-samples, winui/shell/appshellintegration).

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CLASS_E_NOAGGREGATION, E_FAIL, POINT};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoRegisterClassObject,
    CoRevokeClassObject, CoTaskMemFree, IClassFactory, IClassFactory_Impl, REGCLS_SINGLEUSE,
};
use windows::Win32::UI::Shell::{
    IExecuteCommand, IExecuteCommand_Impl, IObjectWithSelection, IObjectWithSelection_Impl, IShellItemArray,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    TranslateMessage,
};
use windows_core::{BOOL, GUID, IUnknown, Interface, PCWSTR, Ref, Result, implement};

/// The class the installer registers as the open verb's `DelegateExecute`.
pub const CLSID: GUID = GUID::from_u128(0xca89377c_91a6_4986_9efd_149f6bf44f21);

type Received = Arc<Mutex<Option<Vec<PathBuf>>>>;

/// Serves the verb once: waits for Explorer to hand over the selection and returns
/// its files (None if nothing came within the time limit).
pub fn receive_files() -> Option<Vec<PathBuf>> {
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.ok().ok()?;
    let received: Received = Arc::new(Mutex::new(None));
    let factory: IClassFactory = Factory { received: received.clone() }.into();
    let cookie = unsafe { CoRegisterClassObject(&CLSID, &factory, CLSCTX_LOCAL_SERVER, REGCLS_SINGLEUSE) }.ok()?;
    let done = || received.lock().map(|r| r.is_some()).unwrap_or(true);
    // COM calls arrive as window messages on this (single-threaded) apartment.
    pump_until(Instant::now() + Duration::from_secs(30), done);
    // A moment for Explorer to finish its calls and release the object.
    pump_until(Instant::now() + Duration::from_millis(200), || false);
    let _ = unsafe { CoRevokeClassObject(cookie) };
    received.lock().ok()?.take()
}

fn pump_until(deadline: Instant, done: impl Fn() -> bool) {
    let mut msg = MSG::default();
    while !done() {
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let wait = left.min(Duration::from_millis(50)).as_millis() as u32;
        unsafe { MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE) };
    }
}

#[implement(IClassFactory)]
struct Factory {
    received: Received,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, iid: *const GUID, object: *mut *mut c_void) -> Result<()> {
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let verb: IUnknown = OpenVerb { received: self.received.clone(), selection: Mutex::new(None) }.into();
        unsafe { verb.query(iid, object).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

#[implement(IExecuteCommand, IObjectWithSelection)]
struct OpenVerb {
    received: Received,
    selection: Mutex<Option<IShellItemArray>>,
}

impl IObjectWithSelection_Impl for OpenVerb_Impl {
    fn SetSelection(&self, items: Ref<IShellItemArray>) -> Result<()> {
        if let Ok(mut s) = self.selection.lock() {
            *s = items.cloned();
        }
        Ok(())
    }

    fn GetSelection(&self, iid: *const GUID, object: *mut *mut c_void) -> Result<()> {
        let s = self.selection.lock().map_err(|_| windows_core::Error::from(E_FAIL))?;
        match s.as_ref() {
            Some(items) => unsafe { items.query(iid, object).ok() },
            None => Err(E_FAIL.into()),
        }
    }
}

impl IExecuteCommand_Impl for OpenVerb_Impl {
    fn SetKeyState(&self, _state: u32) -> Result<()> {
        Ok(())
    }

    fn SetParameters(&self, _parameters: &PCWSTR) -> Result<()> {
        Ok(())
    }

    fn SetPosition(&self, _pt: &POINT) -> Result<()> {
        Ok(())
    }

    fn SetShowWindow(&self, _show: i32) -> Result<()> {
        Ok(())
    }

    fn SetNoShowUI(&self, _no_ui: BOOL) -> Result<()> {
        Ok(())
    }

    fn SetDirectory(&self, _directory: &PCWSTR) -> Result<()> {
        Ok(())
    }

    /// Must not block Explorer: only reads the paths; the files open after this
    /// returns.
    fn Execute(&self) -> Result<()> {
        let items = self.selection.lock().ok().and_then(|s| s.clone()).ok_or(windows_core::Error::from(E_FAIL))?;
        let mut files = Vec::new();
        unsafe {
            for i in 0..items.GetCount()? {
                let Ok(item) = items.GetItemAt(i) else { continue };
                if let Ok(path) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                    if let Ok(s) = path.to_string() {
                        files.push(PathBuf::from(s));
                    }
                    CoTaskMemFree(Some(path.0 as *const c_void));
                }
            }
        }
        if let Ok(mut r) = self.received.lock() {
            *r = Some(files);
        }
        Ok(())
    }
}
