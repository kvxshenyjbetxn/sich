//! xterm mouse reporting encoder (X10/normal, UTF-8 and SGR formats).

use alacritty_terminal::term::TermMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion without any button held.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseMods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Builds the escape sequence for a mouse event, or `None` when the active mouse mode does not
/// want it. `col`/`row` are 0-based viewport cell coordinates.
pub fn mouse_report(
    button: MouseButton,
    action: MouseAction,
    mods: MouseMods,
    col: usize,
    row: usize,
    mode: TermMode,
) -> Option<Vec<u8>> {
    if !mode.intersects(TermMode::MOUSE_MODE) {
        return None;
    }
    let is_wheel = matches!(button, MouseButton::WheelUp | MouseButton::WheelDown);
    match action {
        MouseAction::Motion => {
            let wanted = if button == MouseButton::None {
                mode.contains(TermMode::MOUSE_MOTION)
            } else {
                mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)
            };
            if !wanted || is_wheel {
                return None;
            }
        }
        MouseAction::Release => {
            if is_wheel || button == MouseButton::None {
                return None;
            }
        }
        MouseAction::Press => {
            if button == MouseButton::None {
                return None;
            }
        }
    }

    let sgr = mode.contains(TermMode::SGR_MOUSE);
    let mut b: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
        MouseButton::None => 3,
    };
    if action == MouseAction::Release && !sgr {
        b = 3;
    }
    if action == MouseAction::Motion {
        b += 32;
    }
    if mods.shift {
        b += 4;
    }
    if mods.alt {
        b += 8;
    }
    if mods.ctrl {
        b += 16;
    }

    let x = col as u32 + 1;
    let y = row as u32 + 1;

    if sgr {
        let last = if action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{b};{x};{y}{last}").into_bytes());
    }

    let mut out = vec![0x1b, b'[', b'M'];
    if mode.contains(TermMode::UTF8_MOUSE) {
        for v in [b, x, y] {
            let c = char::from_u32(32 + v)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    } else {
        for v in [b, x, y] {
            let v = 32 + v;
            if v > 255 {
                return None;
            }
            out.push(v as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_MODS: MouseMods = MouseMods { shift: false, alt: false, ctrl: false };

    fn rep(b: MouseButton, a: MouseAction, mods: MouseMods, c: usize, r: usize, m: TermMode) -> Option<Vec<u8>> {
        mouse_report(b, a, mods, c, r, m)
    }

    #[test]
    fn no_mode_none() {
        assert_eq!(rep(MouseButton::Left, MouseAction::Press, NO_MODS, 0, 0, TermMode::empty()), None);
    }

    #[test]
    fn legacy_press_release() {
        let m = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            rep(MouseButton::Left, MouseAction::Press, NO_MODS, 0, 0, m),
            Some(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
        assert_eq!(
            rep(MouseButton::Right, MouseAction::Press, NO_MODS, 4, 9, m),
            Some(vec![0x1b, b'[', b'M', 34, 37, 42])
        );
        assert_eq!(
            rep(MouseButton::Right, MouseAction::Release, NO_MODS, 4, 9, m),
            Some(vec![0x1b, b'[', b'M', 35, 37, 42])
        );
    }

    #[test]
    fn legacy_out_of_range() {
        let m = TermMode::MOUSE_REPORT_CLICK;
        assert!(rep(MouseButton::Left, MouseAction::Press, NO_MODS, 222, 0, m).is_some());
        assert_eq!(rep(MouseButton::Left, MouseAction::Press, NO_MODS, 223, 0, m), None);
    }

    #[test]
    fn utf8_coords() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::UTF8_MOUSE;
        let out = rep(MouseButton::Left, MouseAction::Press, NO_MODS, 299, 0, m).unwrap();
        let mut expect = vec![0x1b, b'[', b'M', 32];
        expect.extend_from_slice("\u{14C}".as_bytes()); // 32 + 300
        expect.push(33);
        assert_eq!(out, expect);
    }

    #[test]
    fn sgr_press_release() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            rep(MouseButton::Left, MouseAction::Press, NO_MODS, 0, 0, m),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
        assert_eq!(
            rep(MouseButton::Middle, MouseAction::Release, NO_MODS, 299, 4, m),
            Some(b"\x1b[<1;300;5m".to_vec())
        );
    }

    #[test]
    fn mods_and_wheel() {
        let m = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let mods = MouseMods { shift: true, alt: true, ctrl: true };
        assert_eq!(
            rep(MouseButton::Left, MouseAction::Press, mods, 1, 1, m),
            Some(b"\x1b[<28;2;2M".to_vec())
        );
        assert_eq!(
            rep(MouseButton::WheelUp, MouseAction::Press, NO_MODS, 0, 0, m),
            Some(b"\x1b[<64;1;1M".to_vec())
        );
        assert_eq!(
            rep(MouseButton::WheelDown, MouseAction::Press, NO_MODS, 0, 0, m),
            Some(b"\x1b[<65;1;1M".to_vec())
        );
        assert_eq!(rep(MouseButton::WheelUp, MouseAction::Release, NO_MODS, 0, 0, m), None);
    }

    #[test]
    fn motion_gating() {
        let click = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let drag = TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE;
        let any = TermMode::MOUSE_MOTION | TermMode::SGR_MOUSE;
        assert_eq!(rep(MouseButton::Left, MouseAction::Motion, NO_MODS, 0, 0, click), None);
        assert_eq!(
            rep(MouseButton::Left, MouseAction::Motion, NO_MODS, 2, 3, drag),
            Some(b"\x1b[<32;3;4M".to_vec())
        );
        assert_eq!(rep(MouseButton::None, MouseAction::Motion, NO_MODS, 0, 0, drag), None);
        assert_eq!(
            rep(MouseButton::None, MouseAction::Motion, NO_MODS, 0, 0, any),
            Some(b"\x1b[<35;1;1M".to_vec())
        );
        assert_eq!(
            rep(MouseButton::Left, MouseAction::Motion, NO_MODS, 0, 0, any),
            Some(b"\x1b[<32;1;1M".to_vec())
        );
    }
}
