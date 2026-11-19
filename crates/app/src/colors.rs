//! Terminal color resolution: palette, indexed colors, truecolor, and OSC overrides.

use std::sync::atomic::{AtomicBool, Ordering};

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// A terminal color scheme: default colors plus the base 16 ANSI palette.
pub struct Scheme {
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    pub ansi: [Rgb; 16],
}

const fn c(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

const ANSI: [Rgb; 16] = [
        c(0x15161e),
        c(0xf7768e),
        c(0x9ece6a),
        c(0xe0af68),
        c(0x7aa2f7),
        c(0xbb9af7),
        c(0x7dcfff),
        c(0xa9b1d6),
        c(0x414868),
        c(0xff899d),
        c(0xb9f27c),
        c(0xffc777),
        c(0x9abdf5),
        c(0xcfb2ff),
        c(0xa4daff),
        c(0xc0caf5),
];

pub const GRAY: Scheme = Scheme {
    fg: c(0xdfe1e5),
    bg: c(0x1e1f22),
    cursor: c(0xdfe1e5),
    selection_bg: c(0x2e436e),
    ansi: ANSI,
};

pub const AMOLED: Scheme = Scheme {
    fg: c(0xe6e6e6),
    bg: c(0x000000),
    cursor: c(0xe6e6e6),
    selection_bg: c(0x1f3a6e),
    ansi: ANSI,
};

static ACTIVE_AMOLED: AtomicBool = AtomicBool::new(false);

/// Selects the terminal scheme matching the app theme.
pub fn set_amoled(on: bool) {
    ACTIVE_AMOLED.store(on, Ordering::Relaxed);
}

/// The scheme for the active theme.
pub fn scheme() -> &'static Scheme {
    if ACTIVE_AMOLED.load(Ordering::Relaxed) { &AMOLED } else { &GRAY }
}

pub fn bg() -> Rgb {
    scheme().bg
}

pub fn fg() -> Rgb {
    scheme().fg
}

pub fn selection_bg() -> Rgb {
    scheme().selection_bg
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 * (1.0 - t) + y as f32 * t).round() as u8;
    Rgb { r: m(a.r, b.r), g: m(a.g, b.g), b: m(a.b, b.b) }
}

fn indexed_default(i: usize) -> Rgb {
    match i {
        0..=15 => scheme().ansi[i],
        16..=231 => {
            let i = i - 16;
            let level = |n: usize| if n == 0 { 0 } else { (55 + 40 * n) as u8 };
            Rgb { r: level(i / 36), g: level((i / 6) % 6), b: level(i % 6) }
        }
        232..=255 => {
            let v = (8 + 10 * (i - 232)) as u8;
            Rgb { r: v, g: v, b: v }
        }
        256 => fg(),
        257 => bg(),
        258 => scheme().cursor,
        // Dim variants of the first 8 colors.
        259..=266 => mix(scheme().ansi[i - 259], bg(), 0.35),
        267 => c(0xffffff),
        268 => mix(fg(), bg(), 0.35),
        _ => fg(),
    }
}

/// Resolve a terminal color to RGB, honoring OSC 4/10/11 overrides.
pub fn resolve(color: Color, overrides: &Colors) -> Rgb {
    match color {
        Color::Spec(rgb) => rgb,
        Color::Indexed(i) => {
            let i = i as usize;
            overrides[i].unwrap_or_else(|| indexed_default(i))
        }
        Color::Named(n) => {
            let i = n as usize;
            overrides[i].unwrap_or_else(|| indexed_default(i))
        }
    }
}

pub fn default_bg(overrides: &Colors) -> Rgb {
    resolve(Color::Named(NamedColor::Background), overrides)
}

pub fn default_fg(overrides: &Colors) -> Rgb {
    resolve(Color::Named(NamedColor::Foreground), overrides)
}

pub fn cursor_color(overrides: &Colors) -> Rgb {
    resolve(Color::Named(NamedColor::Cursor), overrides)
}

pub fn to_gpui(rgb: Rgb) -> gpui_kit::Hsla {
    gpui_kit::rgb(((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32).into()
}
