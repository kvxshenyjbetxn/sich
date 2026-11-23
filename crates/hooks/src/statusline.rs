//! Claude Code's documented statusLine command. Only the normalized limits reach disk.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Stdio};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::usage::{Window, Windows, config_file, unix_seconds};

pub const SNAPSHOT_FILE: &str = "usage/claude.json";
const WRITE_INTERVAL_SECS: i64 = 15;
const COMMAND_PREFIX: &str = "sich-command=";

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub struct Snapshot {
    pub windows: Windows,
    pub session_id: Option<String>,
    pub recorded_at: i64,
}

fn extract(input: &[u8], now: i64) -> Option<Snapshot> {
    let value: Value = serde_json::from_slice(input).ok()?;
    let limits = &value["rate_limits"];
    let window = |key: &str| Window::from_json(&limits[key], "used_percentage", "resets_at");
    let windows = Windows { five_hour: window("five_hour"), weekly: window("seven_day") };
    if windows.is_empty() {
        return None;
    }
    let session_id = value["session_id"].as_str().filter(|id| id.len() <= 128).map(str::to_owned);
    Some(Snapshot { windows, session_id, recorded_at: now })
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("missing parent directory"))?;
    fs::create_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(temp.as_file_mut(), value)?;
    temp.persist(path)?;
    Ok(())
}

pub fn read_snapshot(path: &Path) -> Option<Snapshot> {
    // A valid snapshot is a few hundred bytes. Keep stale or damaged files bounded.
    let mut bytes = Vec::new();
    File::open(path).ok()?.take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn record(path: &Path, input: &[u8], now: i64) -> anyhow::Result<bool> {
    let Some(next) = extract(input, now) else { return Ok(false) };
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("missing usage directory"))?;
    fs::create_dir_all(dir)?;
    // Multiple Claude sessions may relay at once. Lock the comparison and replacement together.
    let lock = OpenOptions::new().write(true).create(true).truncate(false).open(dir.join("claude.lock"))?;
    if lock.try_lock().is_err() {
        return Ok(false);
    }
    if read_snapshot(path).is_some_and(|previous| {
        previous.windows == next.windows
            && previous.session_id == next.session_id
            && (0..WRITE_INTERVAL_SECS).contains(&now.saturating_sub(previous.recorded_at))
    }) {
        return Ok(false);
    }
    write_json(path, &next)?;
    Ok(true)
}

/// Keep the original shell program a single, lossless argument through Claude's outer shell.
/// Quoting the program itself would make its escapes depend on which shell Claude picked.
pub(crate) fn encode_command(command: &str) -> String {
    format!("{COMMAND_PREFIX}{}", URL_SAFE_NO_PAD.encode(command))
}

pub(crate) fn decode_command(argument: &str) -> Option<String> {
    let encoded = argument.strip_prefix(COMMAND_PREFIX)?;
    String::from_utf8(URL_SAFE_NO_PAD.decode(encoded).ok()?).ok()
}

fn original_command(args: &[String]) -> Option<String> {
    let [separator, command @ ..] = args else { return None };
    if separator != "--" || command.is_empty() {
        return None;
    }
    if let [argument] = command
        && argument.starts_with(COMMAND_PREFIX)
    {
        return decode_command(argument);
    }
    Some(command.join(" "))
}

#[cfg(windows)]
pub(crate) fn git_bash() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    if let Some(path) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH").map(PathBuf::from)
        && path.is_file()
    {
        return Some(path);
    }

    // Match a Git for Windows installation, avoiding the WSL launcher in System32.
    let installed = ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|root| PathBuf::from(root).join("Git").join("bin").join("bash.exe"));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let on_path = std::env::split_paths(&path).filter_map(|dir| {
        if dir.join("git.exe").is_file() {
            let root = dir.parent()?;
            Some(root.join("bin").join("bash.exe"))
        } else {
            None
        }
    });
    installed.chain(on_path).find(|path| path.is_file())
}

fn platform_shell(original: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;

        let mut command = if let Some(bash) = git_bash() {
            let mut command = Command::new(bash);
            command.args(["-c", original]);
            command
        } else {
            // PowerShell's encoded-command option accepts the exact UTF-16 shell program.
            let bytes: Vec<_> = original.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-EncodedCommand"]).arg(encoded);
            command
        };
        command.creation_flags(0x0800_0000);
        command
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new("bash");
        command.args(["-c", original]);
        command
    }
}

fn relay(path: Option<&Path>, bytes: &[u8], original: Option<Command>) -> i32 {
    if let Some(path) = path {
        let _ = record(path, bytes, unix_seconds());
    }
    let Some(mut command) = original else { return 0 };
    let Ok(mut child) =
        command.stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn()
    else {
        return 1;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A status line can decide it doesn't need all of stdin.
        let _ = stdin.write_all(bytes);
    }
    match child.wait() {
        Ok(status) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt as _;
                status.code().or_else(|| status.signal().map(|signal| 128 + signal)).unwrap_or(1)
            }
            #[cfg(not(unix))]
            status.code().unwrap_or(1)
        }
        Err(_) => 1,
    }
}

/// Publishes the per-session context status for the GUI. Never fails the status line.
fn record_context(input: &[u8]) {
    let Some(term_id) = std::env::var("SICH_TERM_ID").ok().filter(|id| !id.is_empty()) else { return };
    let Some(dir) = crate::claude_ctx::ctx_dir() else { return };
    let socket = std::env::var("SICH_SOCKET").ok().filter(|s| !s.is_empty());
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64);
    let _ = crate::claude_ctx::record_status(&dir, input, Some(term_id), socket, now_ms);
}

pub fn run(args: &[String]) -> i32 {
    let mut input = Vec::new();
    let _ = io::stdin().lock().read_to_end(&mut input);
    let original = original_command(args).as_deref().map(platform_shell);
    let managed_session = std::env::var("SICH_TERM_ID").ok().and_then(|id| id.parse::<u64>().ok()).is_some();
    let path = managed_session.then(|| config_file(SNAPSHOT_FILE)).flatten();
    record_context(&input);
    relay(path.as_deref(), &input, original)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &[u8] = br#"{
        "session_id":"the-session","transcript_path":"never store this",
        "rate_limits":{
            "five_hour":{"used_percentage":19.5,"resets_at":1000},
            "seven_day":{"used_percentage":104,"resets_at":2000},
            "other":{"used_percentage":60}
        }
    }"#;

    #[test]
    fn extracts_only_documented_windows_and_session_identity() {
        let reading = extract(VALID, 10).unwrap();
        assert_eq!(
            reading,
            Snapshot {
                windows: Windows {
                    five_hour: Some(Window { used_percent: 19.5, resets_at: Some(1000) }),
                    weekly: Some(Window { used_percent: 100.0, resets_at: Some(2000) })
                },
                session_id: Some("the-session".into()),
                recorded_at: 10,
            }
        );
        let one = extract(br#"{"rate_limits":{"seven_day":{"used_percentage":16}}}"#, 0).unwrap();
        assert!(one.windows.five_hour.is_none());
        assert_eq!(one.windows.weekly.unwrap().used_percent, 16.0);
        assert!(extract(br#"{"rate_limits":{}}"#, 0).is_none());
        assert!(extract(b"garbage", 0).is_none());
    }

    #[test]
    fn publishes_atomically_and_throttles_only_identical_readings() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(SNAPSHOT_FILE);
        assert!(!record(&path, b"garbage", 10).unwrap());
        assert!(!path.exists());
        assert!(record(&path, VALID, 10).unwrap());
        assert!(!record(&path, VALID, 24).unwrap());
        assert_eq!(read_snapshot(&path).unwrap().recorded_at, 10);
        assert!(record(&path, VALID, 25).unwrap());
        assert!(record(&path, br#"{"rate_limits":{"seven_day":{"used_percentage":31}}}"#, 26).unwrap());
        let fractional = br#"{"rate_limits":{"seven_day":{"used_percentage":31.1}}}"#;
        assert!(record(&path, fractional, 27).unwrap());
        assert_eq!(read_snapshot(&path).unwrap().windows.weekly.unwrap().used_percent, 31.1);
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("transcript_path"));
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn command_argument_keeps_quotes_pipes_and_expansion_for_the_inner_shell() {
        let original = "printf '%s' \"$HOME\" | cat; echo 'ї \\' %PATH% &'";
        let args = ["--".into(), encode_command(original)];
        assert_eq!(original_command(&args).as_deref(), Some(original));
        assert_eq!(original_command(&["--".into(), "exit".into(), "32".into()]).as_deref(), Some("exit 32"));
        assert!(original_command(&[]).is_none());
    }

    #[test]
    fn relay_passthrough_child() {
        if std::env::var_os("SICH_TEST_STATUSLINE_CHILD").is_none() {
            return;
        }
        let original = if cfg!(windows) && test_uses_powershell() {
            "[Console]::OpenStandardInput().CopyTo([Console]::OpenStandardOutput()); \
             [Console]::Error.Write('relay-stderr-token'); exit 37"
        } else {
            "cat; printf relay-stderr-token >&2; exit 37"
        };
        std::process::exit(run(&["--".into(), encode_command(original)]));
    }

    fn test_uses_powershell() -> bool {
        #[cfg(windows)]
        {
            git_bash().is_none()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    fn capture_passthrough(
        input: &[u8],
        config: &Path,
        in_sich: bool,
        force_powershell: bool,
    ) -> std::process::Output {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "statusline::tests::relay_passthrough_child", "--nocapture"])
            .env("SICH_TEST_STATUSLINE_CHILD", "1")
            .env("SICH_CONFIG_DIR", config)
            .env("SICH_TERM_ID", if in_sich { "77" } else { "" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if cfg!(windows) && force_powershell {
            let system = std::env::var_os("SystemRoot").unwrap();
            command
                .env("PATH", Path::new(&system).join(r"System32\WindowsPowerShell\v1.0"))
                .env_remove("ProgramFiles")
                .env_remove("ProgramFiles(x86)")
                .env_remove("CLAUDE_CODE_GIT_BASH_PATH");
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn original_runs_with_identical_bytes_and_output_on_relay_failure() {
        let invalid: &[u8] = b"not JSON \x00 with raw \xff\r\n";
        for (input, blocked, in_sich) in
            [(VALID, false, true), (invalid, false, true), (VALID, true, true), (VALID, false, false)]
        {
            let config = tempfile::tempdir().unwrap();
            if blocked {
                fs::write(config.path().join("sich"), b"usage directory is blocked").unwrap();
            }
            let output = capture_passthrough(input, config.path(), in_sich, false);
            assert_eq!(output.status.code(), Some(37), "{:?}", output);
            assert!(output.stdout.ends_with(input), "{:?}", output);
            assert_eq!(output.stderr, b"relay-stderr-token");
            let snapshot = config.path().join("sich").join(SNAPSHOT_FILE);
            assert_eq!(snapshot.is_file(), input == VALID && !blocked && in_sich);
            let ctx = config.path().join("sich").join("claude-ctx").join("the-session.status.json");
            assert_eq!(ctx.is_file(), input == VALID && !blocked && in_sich);
            if ctx.is_file() {
                let status: Value = serde_json::from_slice(&fs::read(&ctx).unwrap()).unwrap();
                assert_eq!((status["term_id"].as_str(), status["session_id"].as_str()), (Some("77"), Some("the-session")));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn powershell_fallback_passes_binary_input_stderr_and_exit_code() {
        let config = tempfile::tempdir().unwrap();
        let input = b"raw \r\n\x00\xff";
        let output = capture_passthrough(input, config.path(), true, true);
        assert_eq!(output.status.code(), Some(37), "{:?}", output);
        assert!(output.stdout.ends_with(input), "{:?}", output);
        assert_eq!(output.stderr, b"relay-stderr-token");
    }
}
