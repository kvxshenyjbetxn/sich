//! Terminal color resolution: palette, indexed colors, truecolor, and OSC overrides.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

#[allow(dead_code)] // id/name are table data for a future scheme picker
/// A terminal color scheme: default colors plus the base 16 ANSI palette.
pub struct Scheme {
    pub id: &'static str,
    pub name: &'static str,
    pub fg: Rgb,
    pub bg: Rgb,
    pub cursor: Rgb,
    pub selection_bg: Rgb,
    pub ansi: [Rgb; 16],
}

const fn c(hex: u32) -> Rgb {
    Rgb { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8 }
}

pub const SCHEMES: &[Scheme] = &[
    Scheme {
        id: "tokyo-night",
        name: "Tokyo Night",
        fg: c(0xc0caf5),
        bg: c(0x1a1b26),
        cursor: c(0xc0caf5),
        selection_bg: c(0x33467c),
        ansi: [
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
        ],
    },
    Scheme {
        id: "dracula",
        name: "Dracula",
        fg: c(0xf8f8f2),
        bg: c(0x282a36),
        cursor: c(0xf8f8f2),
        selection_bg: c(0x44475a),
        ansi: [
        c(0x21222c),
        c(0xff5555),
        c(0x50fa7b),
        c(0xf1fa8c),
        c(0xbd93f9),
        c(0xff79c6),
        c(0x8be9fd),
        c(0xf8f8f2),
        c(0x6272a4),
        c(0xff6e6e),
        c(0x69ff94),
        c(0xffffa5),
        c(0xd6acff),
        c(0xff92df),
        c(0xa4ffff),
        c(0xffffff),
        ],
    },
    Scheme {
        id: "one-dark",
        name: "One Dark",
        fg: c(0xabb2bf),
        bg: c(0x282c34),
        cursor: c(0x528bff),
        selection_bg: c(0x3e4451),
        ansi: [
        c(0x282c34),
        c(0xe06c75),
        c(0x98c379),
        c(0xe5c07b),
        c(0x61afef),
        c(0xc678dd),
        c(0x56b6c2),
        c(0xabb2bf),
        c(0x5c6370),
        c(0xe06c75),
        c(0x98c379),
        c(0xe5c07b),
        c(0x61afef),
        c(0xc678dd),
        c(0x56b6c2),
        c(0xffffff),
        ],
    },
    Scheme {
        id: "gruvbox-dark",
        name: "Gruvbox Dark",
        fg: c(0xebdbb2),
        bg: c(0x282828),
        cursor: c(0xebdbb2),
        selection_bg: c(0x504945),
        ansi: [
        c(0x282828),
        c(0xcc241d),
        c(0x98971a),
        c(0xd79921),
        c(0x458588),
        c(0xb16286),
        c(0x689d6a),
        c(0xa89984),
        c(0x928374),
        c(0xfb4934),
        c(0xb8bb26),
        c(0xfabd2f),
        c(0x83a598),
        c(0xd3869b),
        c(0x8ec07c),
        c(0xebdbb2),
        ],
    },
    Scheme {
        id: "catppuccin-mocha",
        name: "Catppuccin Mocha",
        fg: c(0xcdd6f4),
        bg: c(0x1e1e2e),
        cursor: c(0xf5e0dc),
        selection_bg: c(0x585b70),
        ansi: [
        c(0x45475a),
        c(0xf38ba8),
        c(0xa6e3a1),
        c(0xf9e2af),
        c(0x89b4fa),
        c(0xf5c2e7),
        c(0x94e2d5),
        c(0xbac2de),
        c(0x585b70),
        c(0xf38ba8),
        c(0xa6e3a1),
        c(0xf9e2af),
        c(0x89b4fa),
        c(0xf5c2e7),
        c(0x94e2d5),
        c(0xa6adc8),
        ],
    },
    Scheme {
        id: "nord",
        name: "Nord",
        fg: c(0xd8dee9),
        bg: c(0x2e3440),
        cursor: c(0xd8dee9),
        selection_bg: c(0x434c5e),
        ansi: [
        c(0x3b4252),
        c(0xbf616a),
        c(0xa3be8c),
        c(0xebcb8b),
        c(0x81a1c1),
        c(0xb48ead),
        c(0x88c0d0),
        c(0xe5e9f0),
        c(0x4c566a),
        c(0xbf616a),
        c(0xa3be8c),
        c(0xebcb8b),
        c(0x81a1c1),
        c(0xb48ead),
        c(0x8fbcbb),
        c(0xeceff4),
        ],
    },
    Scheme {
        id: "solarized-dark",
        name: "Solarized Dark",
        fg: c(0x839496),
        bg: c(0x002b36),
        cursor: c(0x93a1a1),
        selection_bg: c(0x073642),
        ansi: [
        c(0x073642),
        c(0xdc322f),
        c(0x859900),
        c(0xb58900),
        c(0x268bd2),
        c(0xd33682),
        c(0x2aa198),
        c(0xeee8d5),
        c(0x002b36),
        c(0xcb4b16),
        c(0x586e75),
        c(0x657b83),
        c(0x839496),
        c(0x6c71c4),
        c(0x93a1a1),
        c(0xfdf6e3),
        ],
    },
    Scheme {
        id: "monokai",
        name: "Monokai",
        fg: c(0xf8f8f2),
        bg: c(0x272822),
        cursor: c(0xf8f8f0),
        selection_bg: c(0x49483e),
        ansi: [
        c(0x272822),
        c(0xf92672),
        c(0xa6e22e),
        c(0xf4bf75),
        c(0x66d9ef),
        c(0xae81ff),
        c(0xa1efe4),
        c(0xf8f8f2),
        c(0x75715e),
        c(0xf92672),
        c(0xa6e22e),
        c(0xf4bf75),
        c(0x66d9ef),
        c(0xae81ff),
        c(0xa1efe4),
        c(0xf9f8f5),
        ],
    },
    Scheme {
        id: "github-dark",
        name: "GitHub Dark",
        fg: c(0xc9d1d9),
        bg: c(0x0d1117),
        cursor: c(0xc9d1d9),
        selection_bg: c(0x264f78),
        ansi: [
        c(0x484f58),
        c(0xff7b72),
        c(0x3fb950),
        c(0xd29922),
        c(0x58a6ff),
        c(0xbc8cff),
        c(0x39c5cf),
        c(0xb1bac4),
        c(0x6e7681),
        c(0xffa198),
        c(0x56d364),
        c(0xe3b341),
        c(0x79c0ff),
        c(0xd2a8ff),
        c(0x56d4dd),
        c(0xf0f6fc),
        ],
    },
];

/// The active scheme (Tokyo Night; a scheme picker is not part of v0).
pub fn scheme() -> &'static Scheme {
    &SCHEMES[0]
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
