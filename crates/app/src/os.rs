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

fn quiet(cmd: &mut Command) -> &mut Command {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// The `explorer` argument that selects `path` in its folder: `/select,"C:\dir\file"`.
pub(crate) fn explorer_select_arg(path: &Path) -> String {
    format!("/select,\"{}\"", path.to_string_lossy().replace('/', "\\"))
}

/// Opens a file with the application the OS associates with it.
pub fn open_default(path: &Path) {
    let mut cmd;
    if cfg!(windows) {
        cmd = Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler").arg(path);
    } else if cfg!(target_os = "macos") {
        cmd = Command::new("open");
        cmd.arg(path);
    } else {
        cmd = Command::new("xdg-open");
        cmd.arg(path);
    }
    if let Err(e) = quiet(&mut cmd).spawn() {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}

/// Shows a file in the system file manager (selected where the OS supports it).
pub fn reveal_in_file_manager(path: &Path) {
    let mut cmd;
    if cfg!(windows) {
        cmd = Command::new("explorer");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.raw_arg(explorer_select_arg(path));
        }
    } else if cfg!(target_os = "macos") {
        cmd = Command::new("open");
        cmd.arg("-R").arg(path);
    } else {
        cmd = Command::new("xdg-open");
        cmd.arg(path.parent().unwrap_or(path));
    }
    if let Err(e) = quiet(&mut cmd).spawn() {
        tracing::warn!("could not reveal {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_select_argument_quotes_and_uses_backslashes() {
        assert_eq!(explorer_select_arg(Path::new("C:/Users/me/a b/CLAUDE.md")), r#"/select,"C:\Users\me\a b\CLAUDE.md""#);
        assert_eq!(explorer_select_arg(Path::new(r"C:\x\y.md")), r#"/select,"C:\x\y.md""#);
    }
}
