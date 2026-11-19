//! Taskbar flash when an agent needs the user while the window is in the background (Windows
//! only; a no-op elsewhere). `hwnd_of` needs the UI thread; `flash` can be called from anywhere.

use gpui_kit::Window;

/// Native window handle of `window` (`None` off Windows or when unavailable).
#[cfg(windows)]
pub fn hwnd_of(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
pub fn hwnd_of(_window: &Window) -> Option<isize> {
    None
}

/// Flashes the taskbar button until the window is focused.
#[cfg(windows)]
pub fn flash(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FLASHW_TIMERNOFG, FLASHW_TRAY, FLASHWINFO, FlashWindowEx,
    };
    let info = FLASHWINFO {
        cbSize: size_of::<FLASHWINFO>() as u32,
        hwnd: hwnd as *mut core::ffi::c_void,
        dwFlags: FLASHW_TRAY | FLASHW_TIMERNOFG,
        uCount: 0,
        dwTimeout: 0,
    };
    // SAFETY: `info` is fully initialised and outlives the call.
    unsafe {
        FlashWindowEx(&info);
    }
}

#[cfg(not(windows))]
pub fn flash(_hwnd: isize) {}
