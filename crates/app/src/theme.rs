//! App theme: dark gpui-component theme plus a few fixed surface colors.

use std::time::Duration;

use gpui_kit::App;
use gpui_kit::component::input::CursorBlink;
use gpui_kit::component::{Theme, ThemeMode};

/// Sidebar / status bar background.
pub const SIDEBAR_BG: u32 = 0x16161e;
/// Background of the overlay shown over a terminal (e.g. "process exited").
pub const OVERLAY_BG: u32 = 0x16161e;

pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    // Every caret blink repaints the view; stop blinking when idle so an idle window costs nothing.
    Theme::update(cx, |t| {
        t.cursor_blink = CursorBlink::default().with_idle_timeout(Duration::from_secs(10));
    });
    cx.refresh_windows();
}
