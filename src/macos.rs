//! macOS integration: receiving "open document" requests from Finder.
//!
//! On macOS, double-clicking a PDF (or "Open With") does not pass the file on the
//! command line: the system sends the app an "open documents" Apple Event, also when
//! the app is already running. winit does not handle it, so we install our own
//! handler. It has to be in place when the app finishes launching (the event for the
//! file that started the app arrives right then), so it is installed from the
//! "will finish launching" notification, registered before eframe starts.

use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{AnyThread, define_class, msg_send, sel};
use objc2_foundation::{
    NSAppleEventDescriptor, NSAppleEventManager, NSNotification, NSNotificationCenter, NSObjectProtocol, NSString,
};

/// Files Finder asked us to open, waiting for the app to pick them up.
static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// Wakes the UI when files arrive.
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

const fn four_cc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}
const K_CORE_EVENT_CLASS: u32 = four_cc(b"aevt");
const K_AE_OPEN_DOCUMENTS: u32 = four_cc(b"odoc");
const KEY_DIRECT_OBJECT: u32 = four_cc(b"----");

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop implementation.
    #[unsafe(super(NSObject))]
    #[name = "MagicPdfOpenDocumentsHandler"]
    struct OpenDocumentsHandler;

    impl OpenDocumentsHandler {
        #[unsafe(method(handleOpenDocuments:withReplyEvent:))]
        fn handle_open_documents(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            let Some(list) = event.paramDescriptorForKeyword(KEY_DIRECT_OBJECT) else { return };
            let mut paths = Vec::new();
            // Apple Event lists are 1-based.
            for i in 1..=list.numberOfItems() {
                if let Some(path) = list
                    .descriptorAtIndex(i)
                    .and_then(|item| item.fileURLValue())
                    .and_then(|url| url.path())
                {
                    paths.push(PathBuf::from(path.to_string()));
                }
            }
            if let Ok(mut opened) = OPENED.lock() {
                opened.extend(paths);
            }
            if let Some(wake) = WAKE.get() {
                wake();
            }
        }
    }

    unsafe impl NSObjectProtocol for OpenDocumentsHandler {}
);

fn install_handler() {
    let handler: Retained<OpenDocumentsHandler> = unsafe { msg_send![OpenDocumentsHandler::alloc(), init] };
    let manager = NSAppleEventManager::sharedAppleEventManager();
    // SAFETY: the handler implements the selector with the signature Apple Events
    // expect; it is kept alive for the rest of the process below.
    unsafe {
        manager.setEventHandler_andSelector_forEventClass_andEventID(
            &handler,
            sel!(handleOpenDocuments:withReplyEvent:),
            K_CORE_EVENT_CLASS,
            K_AE_OPEN_DOCUMENTS,
        );
    }
    std::mem::forget(handler);
}

/// Prepares receiving files from Finder. Call before starting eframe.
pub fn install() {
    let center = NSNotificationCenter::defaultCenter();
    let name = NSString::from_str("NSApplicationWillFinishLaunchingNotification");
    let block = RcBlock::new(|_: NonNull<NSNotification>| install_handler());
    // SAFETY: the block only installs the Apple Event handler (main thread).
    let observer = unsafe { center.addObserverForName_object_queue_usingBlock(Some(&name), None, None, &block) };
    // Observe for the lifetime of the process.
    std::mem::forget(observer);
}

/// Sets how to wake the UI when files arrive (call once the app exists).
pub fn set_wake(wake: impl Fn() + Send + Sync + 'static) {
    let _ = WAKE.set(Box::new(wake));
}

/// Files Finder asked to open since the last call.
pub fn take_opened() -> Vec<PathBuf> {
    OPENED.lock().map(|mut v| std::mem::take(&mut *v)).unwrap_or_default()
}
