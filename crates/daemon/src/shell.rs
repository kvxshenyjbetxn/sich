//! Default shell selection.

use std::path::PathBuf;

fn find_in_path(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Windows: `pwsh.exe` if in PATH, else `powershell.exe`. Unix: `$SHELL`, else `/bin/sh`.
pub fn default_shell() -> String {
    if cfg!(windows) {
        let name = if find_in_path("pwsh.exe").is_some() { "pwsh.exe" } else { "powershell.exe" };
        name.to_string()
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }
}

/// Display name of a shell program (file stem).
pub fn shell_title(shell: &str) -> String {
    std::path::Path::new(shell)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| shell.to_string())
}
