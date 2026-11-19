//! Small OS integrations (no console windows).

use std::path::Path;
use std::process::{Command, Stdio};

/// Opens `path` in the system file manager.
pub fn open_in_explorer(path: &Path) {
    let mut cmd = if cfg!(windows) {
        Command::new("explorer")
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    cmd.arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if let Err(e) = cmd.spawn() {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}
