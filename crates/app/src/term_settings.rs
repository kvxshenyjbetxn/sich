//! Terminal appearance settings (font, size, line height) shared by all terminals.

use gpui_kit::{App, Global, SharedString, Window};

pub const MIN_SIZE: f32 = 8.0;
pub const MAX_SIZE: f32 = 32.0;
pub const DEFAULT_SIZE: f32 = 14.0;

#[derive(Clone)]
pub struct TermSettings {
    /// Explicit font family; `None` picks Cascadia Mono, then Consolas.
    pub font: Option<String>,
    pub size: f32,
    pub line_height: f32,
}

impl Global for TermSettings {}

impl Default for TermSettings {
    fn default() -> Self {
        Self { font: None, size: DEFAULT_SIZE, line_height: 1.25 }
    }
}

impl TermSettings {
    /// Resolved font family; falls back to the automatic choice if the font is not installed.
    pub fn family(&self, window: &Window) -> SharedString {
        let names = window.text_system().all_font_names();
        if let Some(f) = &self.font
            && names.iter().any(|n| n == f)
        {
            return f.clone().into();
        }
        if names.iter().any(|n| n == "Cascadia Mono") {
            "Cascadia Mono".into()
        } else {
            "Consolas".into()
        }
    }

    /// Cell height in pixels.
    pub fn line_px(&self) -> f32 {
        (self.size * self.line_height).round()
    }
}

/// Current settings (defaults before `init`).
pub fn get(cx: &App) -> TermSettings {
    cx.try_global::<TermSettings>().cloned().unwrap_or_default()
}

pub fn init(cx: &mut App) {
    cx.set_global(TermSettings::default());
}

pub fn set_size(size: f32, cx: &mut App) {
    let size = size.round().clamp(MIN_SIZE, MAX_SIZE);
    let mut t = get(cx);
    if t.size == size {
        return;
    }
    t.size = size;
    cx.set_global(t);
    cx.refresh_windows();
}

/// Change the font size by `delta` points; `None` resets to the default.
pub fn zoom(delta: Option<f32>, cx: &mut App) {
    let cur = get(cx).size;
    set_size(delta.map_or(DEFAULT_SIZE, |d| cur + d), cx);
}
