//! Decides which path a key press takes to the PTY.
//!
//! On Windows gpui handles `WM_KEYDOWN` in `translate_accelerator` (before `TranslateMessage`).
//! If the key-down listeners stop propagation, `TranslateMessage`/`DispatchMessage` are skipped and
//! no `WM_CHAR` is generated. If propagation continues, `WM_CHAR` follows and reaches the view's
//! input handler (`replace_text_in_range`). So for plain text we must NOT handle the key-down
//! (otherwise injected/IME/dead-key text is lost) and for everything else we handle it and send
//! bytes ourselves. Either way a character reaches the PTY exactly once.

use gpui_kit::Keystroke;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Handle in `key_to_bytes` and stop propagation.
    KeyBytes,
    /// Do not handle; the platform delivers the text to the input handler.
    LetInputHandler,
}

pub fn route_keystroke(ks: &Keystroke, has_input_handler: bool) -> Route {
    if !has_input_handler {
        return Route::KeyBytes;
    }
    let has_text = ks
        .key_char
        .as_deref()
        .is_some_and(|t| !t.is_empty() && !t.chars().all(|c| c.is_control()));
    if !has_text {
        return Route::KeyBytes;
    }
    let m = ks.modifiers;
    if m.platform {
        return Route::KeyBytes;
    }
    // Ctrl+Alt is AltGr on Windows: it produces a real character (delivered via WM_CHAR).
    if m.control && m.alt {
        return Route::LetInputHandler;
    }
    // Plain Ctrl combos and Alt (ESC prefix) combos are terminal shortcuts.
    if m.control || m.alt {
        return Route::KeyBytes;
    }
    Route::LetInputHandler
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::Modifiers;

    fn ks(key: &str, ch: Option<&str>, m: Modifiers) -> Keystroke {
        Keystroke { modifiers: m, key: key.into(), key_char: ch.map(Into::into) }
    }
    fn none() -> Modifiers {
        Modifiers::default()
    }
    fn ctrl() -> Modifiers {
        Modifiers { control: true, ..Default::default() }
    }
    fn alt() -> Modifiers {
        Modifiers { alt: true, ..Default::default() }
    }
    fn shift() -> Modifiers {
        Modifiers { shift: true, ..Default::default() }
    }

    #[test]
    fn latin_letter_goes_to_input_handler() {
        assert_eq!(route_keystroke(&ks("a", Some("a"), none()), true), Route::LetInputHandler);
    }

    #[test]
    fn cyrillic_letter_goes_to_input_handler() {
        assert_eq!(route_keystroke(&ks("ф", Some("ф"), none()), true), Route::LetInputHandler);
        assert_eq!(route_keystroke(&ks("a", Some("ф"), none()), true), Route::LetInputHandler);
    }

    #[test]
    fn shift_letter_goes_to_input_handler() {
        assert_eq!(route_keystroke(&ks("a", Some("A"), shift()), true), Route::LetInputHandler);
    }

    #[test]
    fn space_goes_to_input_handler() {
        assert_eq!(route_keystroke(&ks("space", Some(" "), none()), true), Route::LetInputHandler);
    }

    #[test]
    fn ctrl_c_is_key_bytes() {
        assert_eq!(route_keystroke(&ks("c", None, ctrl()), true), Route::KeyBytes);
        // Even if the layout reported a char with Ctrl held.
        assert_eq!(route_keystroke(&ks("c", Some("c"), ctrl()), true), Route::KeyBytes);
    }

    #[test]
    fn ctrl_shift_v_is_key_bytes() {
        let m = Modifiers { control: true, shift: true, ..Default::default() };
        assert_eq!(route_keystroke(&ks("v", Some("V"), m), true), Route::KeyBytes);
    }

    #[test]
    fn alt_x_is_key_bytes() {
        assert_eq!(route_keystroke(&ks("x", Some("x"), alt()), true), Route::KeyBytes);
    }

    #[test]
    fn special_keys_are_key_bytes() {
        for k in ["enter", "backspace", "tab", "escape", "up", "down", "left", "right", "f5", "delete"] {
            assert_eq!(route_keystroke(&ks(k, None, none()), true), Route::KeyBytes, "{k}");
        }
        // Even if some platform reports a control char for them.
        assert_eq!(route_keystroke(&ks("enter", Some("\r"), none()), true), Route::KeyBytes);
    }

    #[test]
    fn altgr_char_goes_to_input_handler() {
        let m = Modifiers { control: true, alt: true, ..Default::default() };
        assert_eq!(route_keystroke(&ks("2", Some("@"), m), true), Route::LetInputHandler);
    }

    #[test]
    fn ctrl_alt_without_char_is_key_bytes() {
        let m = Modifiers { control: true, alt: true, ..Default::default() };
        assert_eq!(route_keystroke(&ks("a", None, m), true), Route::KeyBytes);
    }

    #[test]
    fn missing_key_char_is_key_bytes() {
        assert_eq!(route_keystroke(&ks("a", None, none()), true), Route::KeyBytes);
        assert_eq!(route_keystroke(&ks("a", Some(""), none()), true), Route::KeyBytes);
    }

    #[test]
    fn platform_modifier_is_key_bytes() {
        let m = Modifiers { platform: true, ..Default::default() };
        assert_eq!(route_keystroke(&ks("a", Some("a"), m), true), Route::KeyBytes);
    }

    #[test]
    fn without_input_handler_everything_is_key_bytes() {
        assert_eq!(route_keystroke(&ks("a", Some("a"), none()), false), Route::KeyBytes);
    }
}
