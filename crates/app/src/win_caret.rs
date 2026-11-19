//! Hidden 1px system caret. We draw our own cursor; the system caret only exists so that
//! accessibility tools and automatic layout switchers (which read `GUITHREADINFO.rcCaret`)
//! recognise the window as a text field. All calls must happen on the UI thread.

/// Caret position in physical client pixels for a cursor cell.
/// `origin` is the grid origin and `cell` the cell size in GPUI pixels; `scale` is the DPI factor.
pub fn caret_px(
    origin: (f32, f32),
    cell: (f32, f32),
    row_col: (usize, usize),
    scale: f32,
) -> (i32, i32) {
    let (row, col) = row_col;
    let x = (origin.0 + cell.0 * col as f32) * scale;
    let y = (origin.1 + cell.1 * row as f32) * scale;
    (x.round() as i32, y.round() as i32)
}

/// Caret height in physical pixels (at least 1).
pub fn caret_height_px(line_h: f32, scale: f32) -> i32 {
    ((line_h * scale).round() as i32).max(1)
}

#[cfg(windows)]
pub use imp::Caret;

#[cfg(windows)]
mod imp {
    use std::cell::Cell;
    use std::ptr::null_mut;

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateCaret, DestroyCaret, SetCaretPos, ShowCaret,
    };

    thread_local! {
        /// Id of the `Caret` that currently owns the thread's system caret (0 = none).
        static OWNER: Cell<u64> = const { Cell::new(0) };
        static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    }

    pub struct Caret {
        id: u64,
        created: bool,
        height: i32,
        last_pos: Option<(i32, i32)>,
    }

    impl Caret {
        pub fn new() -> Self {
            let id = NEXT_ID.with(|n| {
                let v = n.get();
                n.set(v + 1);
                v
            });
            Self { id, created: false, height: 0, last_pos: None }
        }

        fn owns(&self) -> bool {
            self.created && OWNER.with(|o| o.get()) == self.id
        }

        fn destroy(&mut self) {
            if self.owns() {
                unsafe { DestroyCaret() };
                OWNER.with(|o| o.set(0));
            }
            self.created = false;
            self.last_pos = None;
        }

        /// Creates/moves/destroys the caret. `pos` and `height` are physical client pixels.
        pub fn sync(
            &mut self,
            window: &gpui_kit::Window,
            focused: bool,
            pos: (i32, i32),
            height: i32,
        ) {
            if !focused {
                self.destroy();
                return;
            }
            // Another view may have taken the caret over since we created ours.
            if self.created && !self.owns() {
                self.created = false;
                self.last_pos = None;
            }
            if self.created && self.height != height {
                self.destroy();
            }
            if !self.created {
                let Ok(handle) = HasWindowHandle::window_handle(window) else { return };
                let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
                let hwnd = h.hwnd.get() as *mut core::ffi::c_void;
                unsafe {
                    if CreateCaret(hwnd, null_mut(), 1, height) == 0 {
                        return;
                    }
                    ShowCaret(hwnd);
                }
                OWNER.with(|o| o.set(self.id));
                self.created = true;
                self.height = height;
                self.last_pos = None;
            }
            if self.last_pos != Some(pos) && self.owns() {
                unsafe { SetCaretPos(pos.0, pos.1) };
                self.last_pos = Some(pos);
            }
        }
    }

    impl Drop for Caret {
        fn drop(&mut self) {
            self.destroy();
        }
    }
}

/// No-op stand-in on platforms without a system caret.
#[cfg(not(windows))]
pub struct Caret;

#[cfg(not(windows))]
impl Caret {
    pub fn new() -> Self {
        Self
    }

    pub fn sync(&mut self, _window: &gpui_kit::Window, _focused: bool, _pos: (i32, i32), _height: i32) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_at_origin_unscaled() {
        assert_eq!(caret_px((0.0, 0.0), (8.0, 16.0), (0, 0), 1.0), (0, 0));
    }

    #[test]
    fn caret_offsets_by_cell() {
        assert_eq!(caret_px((10.0, 20.0), (8.0, 16.0), (2, 3), 1.0), (34, 52));
    }

    #[test]
    fn caret_applies_scale_factor() {
        assert_eq!(caret_px((10.0, 20.0), (8.0, 16.0), (2, 3), 1.5), (51, 78));
        assert_eq!(caret_px((0.0, 0.0), (7.5, 15.0), (1, 1), 2.0), (15, 30));
    }

    #[test]
    fn caret_rounds_fractional_pixels() {
        assert_eq!(caret_px((0.0, 0.0), (8.4, 17.0), (0, 1), 1.0), (8, 0));
        assert_eq!(caret_px((0.0, 0.0), (8.6, 17.0), (0, 1), 1.0), (9, 0));
    }

    #[test]
    fn caret_height_scales_and_is_positive() {
        assert_eq!(caret_height_px(18.0, 1.0), 18);
        assert_eq!(caret_height_px(18.0, 1.25), 23);
        assert_eq!(caret_height_px(0.0, 1.0), 1);
    }
}
