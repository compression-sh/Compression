//! The X11 counterpart of `wayland_parent.rs`: the portal names the requester's window as
//! `x11:<hex window id>`, and the dialog is marked as transient for it (`WM_TRANSIENT_FOR`) so the
//! window manager keeps it above that window and centers it there.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};

/// The window id a handle names, if it can be one. The requesting program decides what the handle
/// says, so it is checked before it goes to the X server: X resource ids have 29 bits and are never
/// 0 (which is `None` to X), and anything else is no window.
fn parent_id(handle: &str) -> Option<u64> {
    let id = u64::from_str_radix(handle.strip_prefix("x11:")?.trim_start_matches("0x"), 16).ok()?;
    (1..=0x1fff_ffff).contains(&id).then_some(id)
}

/// Errors for requests of ours are told to nobody: a window that is gone by now or that never was
/// is not a reason to end the process, which is what Xlib's own handler does.
unsafe extern "C" fn ignore_error(_: *mut x11_dl::xlib::Display, _: *mut x11_dl::xlib::XErrorEvent) -> std::os::raw::c_int {
    0
}

/// Marks `window` as a child of the window `handle` names; false when this is not an X11 window,
/// the handle is of another kind or names no possible window. The dialog is then an ordinary window.
pub fn attach<W: HasWindowHandle + HasDisplayHandle>(window: &W, handle: &str) -> bool {
    let Some(parent) = parent_id(handle) else { return false };
    let (Ok(display), Ok(own)) = (window.display_handle(), window.window_handle()) else { return false };
    let (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(own)) = (display.as_raw(), own.as_raw()) else { return false };
    let Some(display) = display.display else { return false };
    let Ok(xlib) = x11_dl::xlib::Xlib::open() else { return false };
    // SAFETY: the display and window come from the live window winit created, which outlives this
    // call; the calls only read them and write one property. A window that does not exist makes the
    // server answer with an error, which the handler swallows while it is installed; the sync makes
    // that answer arrive before the previous handler is put back.
    unsafe {
        let previous = (xlib.XSetErrorHandler)(Some(ignore_error));
        (xlib.XSetTransientForHint)(display.as_ptr().cast(), own.window, parent);
        (xlib.XSync)(display.as_ptr().cast(), 0);
        (xlib.XSetErrorHandler)(previous);
    }
    true
}

