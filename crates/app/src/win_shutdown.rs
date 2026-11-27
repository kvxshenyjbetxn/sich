//! Windows shutdown guard. During a reboot Windows kills the agents and shells before the GUI,
//! so the GUI would see every terminal end and save an empty session. A hidden top-level window
//! receives `WM_QUERYENDSESSION` / `WM_ENDSESSION` and freezes session saving. Off Windows this
//! is a no-op. The decision logic is pure and unit-tested.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const WM_QUERYENDSESSION: u32 = 0x0011;
const WM_ENDSESSION: u32 = 0x0016;

/// A freeze older than this counts as not frozen (the process survived the shutdown somehow).
const FREEZE_MAX_AGE_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FreezeChange {
    Freeze,
    Unfreeze,
}

/// What a window message does to the freeze state; `None` = nothing.
fn freeze_action(msg: u32, wparam: usize) -> Option<FreezeChange> {
    match msg {
        WM_QUERYENDSESSION => Some(FreezeChange::Freeze),
        WM_ENDSESSION if wparam != 0 => Some(FreezeChange::Freeze),
        WM_ENDSESSION => Some(FreezeChange::Unfreeze),
        _ => None,
    }
}

/// `frozen_at` is the freeze time in ms (0 = never frozen); `now` is in the same clock.
fn is_frozen(frozen_at: u64, now: u64) -> bool {
    frozen_at != 0 && now.saturating_sub(frozen_at) < FREEZE_MAX_AGE_MS
}

static FROZEN_AT: AtomicU64 = AtomicU64::new(0);
static CLOCK: OnceLock<Instant> = OnceLock::new();

/// Milliseconds since the first call, never 0.
fn now_ms() -> u64 {
    CLOCK.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

fn apply(change: FreezeChange) {
    match change {
        FreezeChange::Freeze => FROZEN_AT.store(now_ms(), Ordering::SeqCst),
        FreezeChange::Unfreeze => FROZEN_AT.store(0, Ordering::SeqCst),
    }
}

/// True while Windows is shutting down: the session on disk must not be rewritten.
pub fn session_frozen() -> bool {
    is_frozen(FROZEN_AT.load(Ordering::SeqCst), now_ms())
}

#[cfg(not(windows))]
pub fn install() {}

/// Asks to be queried before other apps and starts the hidden window that watches for shutdown.
#[cfg(windows)]
pub fn install() {
    use windows_sys::Win32::System::Threading::SetProcessShutdownParameters;
    const SHUTDOWN_NORETRY: u32 = 1;
    let _ = now_ms();
    // SAFETY: plain FFI call with value arguments.
    if unsafe { SetProcessShutdownParameters(0x3FF, SHUTDOWN_NORETRY) } == 0 {
        tracing::warn!("SetProcessShutdownParameters failed");
    }
    if let Err(e) = std::thread::Builder::new().name("win-shutdown".into()).spawn(message_loop) {
        tracing::warn!("failed to start the shutdown watcher: {e}");
    }
}

#[cfg(windows)]
unsafe extern "system" fn wnd_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    match freeze_action(msg, wparam) {
        Some(change) => {
            apply(change);
            // Never block the shutdown: TRUE for the query, 0 for the end notification.
            if msg == WM_QUERYENDSESSION { 1 } else { 0 }
        }
        // SAFETY: forwarding the original arguments of this window procedure.
        None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(windows)]
fn message_loop() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DispatchMessageW, GetMessageW, MSG, RegisterClassW, TranslateMessage, WNDCLASSW,
    };
    let class: Vec<u16> = "SichShutdownWatcher\0".encode_utf16().collect();
    // SAFETY: all pointers passed to the Win32 calls outlive them; the class name is NUL-terminated.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        if RegisterClassW(&wc) == 0 {
            tracing::warn!("failed to register the shutdown watcher window class");
            return;
        }
        // Top-level and never shown: message-only windows do not receive the session messages.
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), class.as_ptr(), 0, 0, 0, 0, 0,
            std::ptr::null_mut(), std::ptr::null_mut(), instance, std::ptr::null(),
        );
        if hwnd.is_null() {
            tracing::warn!("failed to create the shutdown watcher window");
            return;
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_end_session_freeze_and_cancel_unfreezes() {
        assert_eq!(freeze_action(WM_QUERYENDSESSION, 0), Some(FreezeChange::Freeze));
        assert_eq!(freeze_action(WM_QUERYENDSESSION, 1), Some(FreezeChange::Freeze));
        assert_eq!(freeze_action(WM_ENDSESSION, 1), Some(FreezeChange::Freeze));
        assert_eq!(freeze_action(WM_ENDSESSION, 0), Some(FreezeChange::Unfreeze));
        assert_eq!(freeze_action(0x0001, 1), None);
        assert_eq!(freeze_action(0, 0), None);
    }

    #[test]
    fn freeze_expires_after_two_minutes() {
        assert!(!is_frozen(0, 1_000_000), "never frozen");
        assert!(is_frozen(1_000, 1_000));
        assert!(is_frozen(1_000, 1_000 + FREEZE_MAX_AGE_MS - 1));
        assert!(!is_frozen(1_000, 1_000 + FREEZE_MAX_AGE_MS));
        assert!(is_frozen(5_000, 1_000), "a clock going backwards neither panics nor thaws");
    }

    #[test]
    fn apply_toggles_the_global_state() {
        apply(FreezeChange::Freeze);
        assert!(session_frozen());
        apply(FreezeChange::Unfreeze);
        assert!(!session_frozen());
    }
}
