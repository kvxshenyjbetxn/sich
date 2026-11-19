//! Hiding the main window to the tray and bringing it back (Windows only; no-ops elsewhere).
//! Call from the UI thread.

/// Hides the window (it keeps running; the tray icon brings it back).
#[cfg(windows)]
pub fn hide(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
    // SAFETY: plain Win32 call on a window handle owned by this process.
    unsafe {
        ShowWindow(hwnd as *mut core::ffi::c_void, SW_HIDE);
    }
}

/// Shows the window again, restoring it when minimized, and brings it to the foreground.
#[cfg(windows)]
pub fn show(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IsIconic, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow,
    };
    let h = hwnd as *mut core::ffi::c_void;
    // SAFETY: plain Win32 calls on a window handle owned by this process.
    unsafe {
        ShowWindow(h, if IsIconic(h) != 0 { SW_RESTORE } else { SW_SHOW });
        SetForegroundWindow(h);
    }
}

#[cfg(not(windows))]
pub fn hide(_hwnd: isize) {}

#[cfg(not(windows))]
pub fn show(_hwnd: isize) {}
