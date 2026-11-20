//! Client-side terminal model: daemon output -> alacritty_terminal state, plus key/mouse mapping.

mod diacritics;
mod images;
mod inline;
mod intercept;
mod iterm;
mod keys;
mod kitty;
mod mouse;
mod placeholder;
mod session;

pub use images::{ImageData, Placement};
pub use keys::{KeyMods, key_to_bytes, paste_bytes};
pub use mouse::{MouseAction, MouseButton, MouseMods, mouse_report};
pub use placeholder::{PLACEHOLDER, RawCell, Resolved, parse_cell, strip_placeholders};
pub use session::{ResizeNotifier, Sender, TermSession, osc52_write, set_osc52_write};

pub use alacritty_terminal;
