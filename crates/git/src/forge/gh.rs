use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::ForgeError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `gh` with the given arguments. A trait so tests can script the output.
pub trait GhExec: Send + Sync {
    fn run(&self, dir: &Path, args: &[String], timeout: Duration) -> Result<GhOutput, ForgeError>;
}

/// The real, installed GitHub CLI. Override the program with the `SICH_GH` env var.
#[derive(Clone, Copy, Debug, Default)]
pub struct GhCli;

impl GhExec for GhCli {
    fn run(&self, dir: &Path, args: &[String], timeout: Duration) -> Result<GhOutput, ForgeError> {
        let program = std::env::var_os("SICH_GH")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "gh".into());
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("GH_SPINNER_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                ForgeError::GhMissing
            } else {
                ForgeError::Failed(format!("Failed to run gh: {error}"))
            }
        })?;
        let stdout = drain(child.stdout.take().expect("piped stdout"));
        let stderr = drain(child.stderr.take().expect("piped stderr"));
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ForgeError::TimedOut);
                }
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ForgeError::Failed(format!("Failed to wait for gh: {error}")));
                }
            }
        };
        let join = |handle: thread::JoinHandle<Vec<u8>>| handle.join().unwrap_or_default();
        Ok(GhOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&join(stdout)).into_owned(),
            stderr: String::from_utf8_lossy(&join(stderr)).into_owned(),
        })
    }
}

fn drain(mut stream: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        bytes
    })
}

/// Maps a failed `gh` run to an error. Exit code 4 is gh's "authentication required".
pub(crate) fn classify(subcommand: &str, output: &GhOutput) -> ForgeError {
    let lower = output.stderr.to_ascii_lowercase();
    if output.code == Some(4)
        || lower.contains("gh auth login")
        || lower.contains("not logged into")
        || lower.contains("authentication required")
    {
        return ForgeError::NotAuthenticated;
    }
    let lines: Vec<&str> = output
        .stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        let code = output
            .code
            .map_or_else(|| "terminated".to_owned(), |code| code.to_string());
        ForgeError::Failed(format!("gh {subcommand} failed ({code})"))
    } else {
        ForgeError::Failed(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(code: Option<i32>, stderr: &str) -> GhOutput {
        GhOutput {
            code,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn exit_code_4_is_not_authenticated() {
        assert_eq!(classify("pr", &out(Some(4), "")), ForgeError::NotAuthenticated);
    }

    #[test]
    fn auth_messages_are_not_authenticated() {
        for text in [
            "To get started with GitHub CLI, please run:  gh auth login",
            "You are not logged into any GitHub hosts.",
            "Authentication required",
        ] {
            assert_eq!(classify("pr", &out(Some(1), text)), ForgeError::NotAuthenticated);
        }
    }

    #[test]
    fn other_failures_keep_first_three_lines() {
        let error = classify("pr", &out(Some(1), "\none\n\ntwo\nthree\nfour\n"));
        assert_eq!(error, ForgeError::Failed("one\ntwo\nthree".into()));
    }

    #[test]
    fn empty_stderr_names_subcommand_and_code() {
        assert_eq!(
            classify("pr", &out(Some(2), "  \n")),
            ForgeError::Failed("gh pr failed (2)".into())
        );
        assert_eq!(
            classify("run", &out(None, "")),
            ForgeError::Failed("gh run failed (terminated)".into())
        );
    }
}
