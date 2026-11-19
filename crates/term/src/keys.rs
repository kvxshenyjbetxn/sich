//! Pure key -> terminal byte mapping (xterm style).

use alacritty_terminal::term::TermMode;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl KeyMods {
    /// xterm modifier parameter (1 = none).
    fn param(self) -> u8 {
        1 + self.shift as u8 + 2 * self.alt as u8 + 4 * self.ctrl as u8
    }

    fn any(self) -> bool {
        self.ctrl || self.alt || self.shift
    }
}

fn csi_final(mods: KeyMods, app_cursor: bool, fin: char) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[1;{}{}", mods.param(), fin).into_bytes()
    } else if app_cursor {
        format!("\x1bO{fin}").into_bytes()
    } else {
        format!("\x1b[{fin}").into_bytes()
    }
}

fn csi_tilde(n: u8, mods: KeyMods) -> Vec<u8> {
    if mods.any() {
        format!("\x1b[{};{}~", n, mods.param()).into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

fn with_alt(mods: KeyMods, mut bytes: Vec<u8>) -> Vec<u8> {
    if mods.alt {
        bytes.insert(0, 0x1b);
    }
    bytes
}

/// Map a key press to the bytes to send. `key` is the logical key name
/// (gpui style: "enter", "up", "f5", "a", "space"), `key_char` the typed text if any.
/// APP_KEYPAD is accepted via `mode` but gpui reports no distinct keypad keys, so it has no effect yet.
pub fn key_to_bytes(
    key: &str,
    key_char: Option<&str>,
    mods: KeyMods,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let bytes = match key {
        "enter" => with_alt(mods, vec![b'\r']),
        "backspace" => {
            if mods.ctrl {
                with_alt(mods, vec![0x08])
            } else {
                with_alt(mods, vec![0x7f])
            }
        }
        "tab" => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                with_alt(mods, vec![b'\t'])
            }
        }
        "escape" => with_alt(mods, vec![0x1b]),
        "up" => csi_final(mods, app_cursor, 'A'),
        "down" => csi_final(mods, app_cursor, 'B'),
        "right" => csi_final(mods, app_cursor, 'C'),
        "left" => csi_final(mods, app_cursor, 'D'),
        "home" => csi_final(mods, app_cursor, 'H'),
        "end" => csi_final(mods, app_cursor, 'F'),
        "insert" => csi_tilde(2, mods),
        "delete" => csi_tilde(3, mods),
        "pageup" => csi_tilde(5, mods),
        "pagedown" => csi_tilde(6, mods),
        "f1" | "f2" | "f3" | "f4" => {
            let fin = match key {
                "f1" => 'P',
                "f2" => 'Q',
                "f3" => 'R',
                _ => 'S',
            };
            if mods.any() {
                format!("\x1b[1;{}{}", mods.param(), fin).into_bytes()
            } else {
                format!("\x1bO{fin}").into_bytes()
            }
        }
        "f5" => csi_tilde(15, mods),
        "f6" => csi_tilde(17, mods),
        "f7" => csi_tilde(18, mods),
        "f8" => csi_tilde(19, mods),
        "f9" => csi_tilde(20, mods),
        "f10" => csi_tilde(21, mods),
        "f11" => csi_tilde(23, mods),
        "f12" => csi_tilde(24, mods),
        _ => return printable(key, key_char, mods),
    };
    Some(bytes)
}

fn printable(key: &str, key_char: Option<&str>, mods: KeyMods) -> Option<Vec<u8>> {
    if mods.ctrl {
        let c = if key == "space" {
            Some(' ')
        } else {
            let mut it = key.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        if let Some(c) = c {
            let code = match c.to_ascii_lowercase() {
                ' ' | '@' | '2' => Some(0),
                l @ 'a'..='z' => Some(l as u8 - b'a' + 1),
                '[' | '3' => Some(0x1b),
                '\\' | '4' => Some(0x1c),
                ']' | '5' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '7' | '/' => Some(0x1f),
                '8' | '?' => Some(0x7f),
                _ => None,
            };
            if let Some(b) = code {
                return Some(with_alt(mods, vec![b]));
            }
        }
        return None;
    }

    if let Some(text) = key_char
        && !text.is_empty()
        && !text.chars().all(|c| c.is_control())
    {
        return Some(with_alt(mods, text.as_bytes().to_vec()));
    }
    if key == "space" {
        return Some(with_alt(mods, vec![b' ']));
    }
    // No key_char: fall back to the key itself when it is a single character.
    let mut it = key.chars();
    if let (Some(c), None) = (it.next(), it.next()) {
        let c = if mods.shift { c.to_ascii_uppercase() } else { c };
        let mut buf = [0u8; 4];
        return Some(with_alt(mods, c.encode_utf8(&mut buf).as_bytes().to_vec()));
    }
    None
}

/// Bytes to send for pasted text. Newlines become CR; wrapped in bracketed paste when enabled.
pub fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
    let mut s = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // Prevent the pasted text from terminating the paste early.
        s.retain(|c| c != '\x1b');
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(s.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyMods = KeyMods { ctrl: false, alt: false, shift: false };
    const CTRL: KeyMods = KeyMods { ctrl: true, alt: false, shift: false };
    const ALT: KeyMods = KeyMods { ctrl: false, alt: true, shift: false };
    const SHIFT: KeyMods = KeyMods { ctrl: false, alt: false, shift: true };

    fn k(key: &str, ch: Option<&str>, m: KeyMods, mode: TermMode) -> Vec<u8> {
        key_to_bytes(key, ch, m, mode).unwrap()
    }

    #[test]
    fn basic_keys() {
        let m = TermMode::NONE;
        assert_eq!(k("a", Some("a"), NONE, m), b"a");
        assert_eq!(k("enter", None, NONE, m), b"\r");
        assert_eq!(k("backspace", None, NONE, m), b"\x7f");
        assert_eq!(k("tab", None, SHIFT, m), b"\x1b[Z");
        assert_eq!(k("escape", None, NONE, m), b"\x1b");
        assert_eq!(k("space", Some(" "), NONE, m), b" ");
    }

    #[test]
    fn arrows() {
        assert_eq!(k("up", None, NONE, TermMode::NONE), b"\x1b[A");
        assert_eq!(k("up", None, NONE, TermMode::APP_CURSOR), b"\x1bOA");
        assert_eq!(k("right", None, CTRL, TermMode::APP_CURSOR), b"\x1b[1;5C");
        assert_eq!(k("left", None, ALT, TermMode::NONE), b"\x1b[1;3D");
    }

    #[test]
    fn nav_and_function() {
        let m = TermMode::NONE;
        assert_eq!(k("delete", None, NONE, m), b"\x1b[3~");
        assert_eq!(k("pageup", None, SHIFT, m), b"\x1b[5;2~");
        assert_eq!(k("f1", None, NONE, m), b"\x1bOP");
        assert_eq!(k("f5", None, NONE, m), b"\x1b[15~");
        assert_eq!(k("f12", None, CTRL, m), b"\x1b[24;5~");
    }

    #[test]
    fn ctrl_and_alt() {
        let m = TermMode::NONE;
        assert_eq!(k("c", None, CTRL, m), b"\x03");
        assert_eq!(k("space", None, CTRL, m), b"\0");
        assert_eq!(k("[", None, CTRL, m), b"\x1b");
        assert_eq!(k("x", Some("x"), ALT, m), b"\x1bx");
    }

    #[test]
    fn paste() {
        assert_eq!(paste_bytes("a\r\nb\nc", TermMode::NONE), b"a\rb\rc");
        assert_eq!(paste_bytes("hi", TermMode::BRACKETED_PASTE), b"\x1b[200~hi\x1b[201~");
    }
}
