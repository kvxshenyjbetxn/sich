//! `sich-cli`: a console-subsystem relay for the CLI subcommands of `sich`.
//!
//! `sich.exe` is a GUI-subsystem binary in release builds, so PowerShell neither waits for it nor
//! shows its output nor sets `$LASTEXITCODE`. This tiny binary (std only, no GPUI) runs the GUI
//! exe next to it with the same arguments, copies its stdout/stderr to ours and exits with its
//! exit code. Only the CLI subcommands are relayed; it never starts the GUI.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GUI_EXE: &str = if cfg!(windows) { "sich.exe" } else { "sich" };
const EXE_ENV: &str = "SICH_EXE";

const USAGE: &str = "usage: sich-cli orch <command> [args]   (see: sich-cli orch --help)\n       sich-cli daemon [start|stop]\n";

/// Subcommands that are relayed to the GUI exe. Anything else (including no arguments, which
/// would start the GUI) is refused.
fn is_cli_subcommand(args: &[OsString]) -> bool {
    matches!(args.first().and_then(|a| a.to_str()), Some("orch" | "daemon"))
}

/// The exe to relay to: the `SICH_EXE` override when non-empty, else `sich.exe` next to us.
fn relay_target(env_override: Option<OsString>, current_exe: &Path) -> PathBuf {
    match env_override.filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => current_exe.with_file_name(GUI_EXE),
    }
}

fn pump(mut from: impl Read, mut to: impl Write) {
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    // Our reader went away: keep draining so the child never blocks.
                    let _ = io::copy(&mut from, &mut io::sink());
                    break;
                }
                let _ = to.flush();
            }
        }
    }
}

fn run() -> i32 {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if !is_cli_subcommand(&args) {
        eprint!("{USAGE}");
        return 2;
    }
    let current = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sich-cli: cannot locate itself: {e}");
            return 1;
        }
    };
    let target = relay_target(std::env::var_os(EXE_ENV), &current);
    let mut child = match Command::new(&target)
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sich-cli: cannot start {}: {e}", target.display());
            return 1;
        }
    };
    let out = child.stdout.take().map(|p| std::thread::spawn(move || pump(p, io::stdout())));
    let err = child.stderr.take().map(|p| std::thread::spawn(move || pump(p, io::stderr())));
    let status = child.wait();
    for t in [out, err].into_iter().flatten() {
        let _ = t.join();
    }
    match status {
        Ok(s) => s.code().unwrap_or_else(|| {
            eprintln!("sich-cli: {} was terminated", target.display());
            1
        }),
        Err(e) => {
            eprintln!("sich-cli: waiting for {} failed: {e}", target.display());
            1
        }
    }
}

fn main() {
    std::process::exit(run());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn classifies_cli_subcommands() {
        assert!(is_cli_subcommand(&a(&["orch", "list"])));
        assert!(is_cli_subcommand(&a(&["daemon", "stop"])));
        assert!(!is_cli_subcommand(&a(&[])));
        assert!(!is_cli_subcommand(&a(&["--takeover"])));
        assert!(!is_cli_subcommand(&a(&["hook", "claude"])));
        assert!(!is_cli_subcommand(&a(&["bogus"])));
    }

    #[test]
    fn relay_target_prefers_env_override_then_sibling() {
        let cur = Path::new("C:/Apps/Sich/sich-cli.exe");
        assert_eq!(relay_target(Some("D:/x/fake.exe".into()), cur), PathBuf::from("D:/x/fake.exe"));
        assert_eq!(relay_target(Some("".into()), cur), cur.with_file_name(GUI_EXE));
        assert_eq!(relay_target(None, cur), cur.with_file_name(GUI_EXE));
    }
}
