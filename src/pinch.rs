//! Touchpad pinch-to-zoom on Wayland.
//!
//! winit only reports pinch gestures on macOS and iOS (Windows touchpads send
//! Ctrl+wheel instead), so on Wayland a touchpad pinch would never reach egui. This
//! module listens to the `zwp_pointer_gestures_v1` protocol on the window's own
//! Wayland connection, using a separate event queue served by a small thread, and
//! hands the result to egui as `Event::Zoom` (see `App::raw_input_hook`). egui then
//! zooms like Ctrl+wheel: around the pointer, in the document or the thumbnails.
//!
//! Under X11, or when the compositor lacks the protocol, nothing happens.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use eframe::egui;
use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_pinch_v1::{self, ZwpPointerGesturePinchV1},
    zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
};

/// Turns the absolute `scale` of a pinch (1.0 at its start) into zoom factors
/// relative to the previous update.
#[derive(Default)]
struct PinchTracker {
    last: Option<f64>,
}

impl PinchTracker {
    fn begin(&mut self) {
        self.last = Some(1.0);
    }

    /// The zoom factor since the last update, if there is a change.
    fn update(&mut self, scale: f64) -> Option<f32> {
        let last = self.last?;
        if !scale.is_finite() || scale <= 0.0 {
            return None;
        }
        self.last = Some(scale);
        let factor = (scale / last) as f32;
        (factor.is_finite() && factor != 1.0).then_some(factor)
    }

    fn end(&mut self) {
        self.last = None;
    }
}

/// Zoom collected by the listener thread and not yet given to egui (1.0 = none).
type Pending = Arc<Mutex<f32>>;

/// The running pinch listener. Stop it (or drop it) before the window closes.
pub struct Pinch {
    pending: Pending,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pinch {
    /// Starts listening if the app runs on Wayland and the compositor supports
    /// pointer gestures.
    pub fn start(cc: &eframe::CreationContext<'_>) -> Option<Self> {
        let RawDisplayHandle::Wayland(handle) = cc.display_handle().ok()?.as_raw() else {
            return None;
        };
        // SAFETY: the display belongs to the window, which outlives the listener:
        // `stop` is called from `App::on_exit`, before eframe closes the window, and
        // joins the thread. The connection is foreign, so dropping it does not
        // disconnect the display.
        let backend =
            unsafe { wayland_backend::sys::client::Backend::from_foreign_display(handle.display.as_ptr().cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<State>(&conn).ok()?;
        let qh = queue.handle();
        let gestures: ZwpPointerGesturesV1 = globals.bind(&qh, 1..=1, ()).ok()?;
        // Pointers are created when a seat announces one (`Capabilities`).
        let seats: Vec<wl_seat::WlSeat> = globals
            .contents()
            .clone_list()
            .iter()
            .filter(|g| g.interface == "wl_seat")
            .map(|g| globals.registry().bind(g.name, g.version.min(5), &qh, ()))
            .collect();
        if seats.is_empty() {
            return None;
        }

        let pending: Pending = Arc::new(Mutex::new(1.0));
        let stop = Arc::new(AtomicBool::new(false));
        let mut state = State {
            gestures,
            pending: pending.clone(),
            ctx: cc.egui_ctx.clone(),
            tracker: PinchTracker::default(),
            _seats: seats,
        };
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("wayland-pinch".into())
            .spawn(move || run(queue, &mut state, &thread_stop))
            .ok()?;
        Some(Self { pending, stop, thread: Some(thread) })
    }

    /// The zoom factor gathered since the last call, if any.
    pub fn take_zoom(&self) -> Option<f32> {
        let mut pending = self.pending.lock().ok()?;
        let factor = std::mem::replace(&mut *pending, 1.0);
        (factor != 1.0).then_some(factor)
    }

    /// Stops the listener thread and waits for it (at most one poll interval).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Pinch {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How long the listener waits for the socket before checking whether to stop.
const POLL_MS: i32 = 100;

fn run(mut queue: wayland_client::EventQueue<State>, state: &mut State, stop: &AtomicBool) {
    let ok = |r: Result<usize, wayland_client::backend::WaylandError>| match r {
        Ok(_) => true,
        Err(wayland_client::backend::WaylandError::Io(e)) => e.kind() == std::io::ErrorKind::WouldBlock,
        Err(_) => false,
    };
    while !stop.load(Ordering::Relaxed) {
        if queue.dispatch_pending(state).is_err() || !ok(queue.flush().map(|_| 0)) {
            return;
        }
        // Read together with winit's thread: libwayland sorts the events into queues.
        let Some(guard) = queue.prepare_read() else { continue };
        let mut fd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut fd, 1, POLL_MS) };
        if ready > 0 && !ok(guard.read()) {
            return;
        }
        // Otherwise dropping the guard cancels the read.
    }
}

struct State {
    gestures: ZwpPointerGesturesV1,
    pending: Pending,
    ctx: egui::Context,
    tracker: PinchTracker,
    _seats: Vec<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event
            && caps.contains(wl_seat::Capability::Pointer)
        {
            let pointer = seat.get_pointer(qh, ());
            state.gestures.get_pinch_gesture(&pointer, qh, ());
        }
    }
}

impl Dispatch<ZwpPointerGesturePinchV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpPointerGesturePinchV1,
        event: zwp_pointer_gesture_pinch_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwp_pointer_gesture_pinch_v1::Event;
        match event {
            Event::Begin { .. } => state.tracker.begin(),
            Event::Update { scale, .. } => {
                if let Some(factor) = state.tracker.update(scale) {
                    if let Ok(mut pending) = state.pending.lock() {
                        *pending *= factor;
                    }
                    state.ctx.request_repaint();
                }
            }
            Event::End { .. } => state.tracker.end(),
            _ => {}
        }
    }
}

// Pointer events (enter, motion, buttons) reach egui through winit already.
delegate_noop!(State: ignore wl_pointer::WlPointer);
delegate_noop!(State: ZwpPointerGesturesV1);

#[cfg(test)]
mod tests {
    use super::PinchTracker;

    #[test]
    fn pinch_scale_becomes_relative_zoom_steps() {
        let mut t = PinchTracker::default();
        // Updates outside a pinch are ignored.
        assert_eq!(t.update(2.0), None);
        t.begin();
        assert_eq!(t.update(1.25), Some(1.25));
        assert_eq!(t.update(1.5), Some(1.2));
        // No change, no zoom.
        assert_eq!(t.update(1.5), None);
        // Pinching in again.
        assert_eq!(t.update(0.75), Some(0.5));
        // Garbage from the compositor does not zoom.
        assert_eq!(t.update(0.0), None);
        assert_eq!(t.update(f64::NAN), None);
        t.end();
        assert_eq!(t.update(3.0), None);

        // The steps of a whole pinch multiply up to its final scale.
        t.begin();
        let total: f32 = [1.1, 1.3, 1.2, 1.8].iter().filter_map(|&s| t.update(s)).product();
        assert!((total - 1.8).abs() < 1e-5);
    }
}
