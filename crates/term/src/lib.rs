//! Client-side terminal model: daemon output -> alacritty_terminal state, plus key/mouse mapping.

mod keys;
mod mouse;
mod session;

pub use keys::{KeyMods, key_to_bytes, paste_bytes};
pub use mouse::{MouseAction, MouseButton, MouseMods, mouse_report};
pub use session::{ResizeNotifier, Sender, TermSession, osc52_write, set_osc52_write};

pub use alacritty_terminal;
