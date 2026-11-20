//! Bounded, newline-delimited JSON-RPC for Codex's stdio app-server.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, bail};
use futures::StreamExt as _;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio_util::codec::{FramedRead, LinesCodec};

pub(crate) const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn bounded<T>(operation: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let result = runtime.block_on(async {
        tokio::time::timeout(TIMEOUT, operation).await.context("Codex app-server timed out")?
    });
    runtime.shutdown_background();
    result
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({ "id": id, "method": method, "params": params })
}

fn initialize_params() -> Value {
    json!({ "clientInfo": { "name": "sich", "version": env!("CARGO_PKG_VERSION") } })
}

fn initialized() -> Value {
    json!({ "method": "initialized" })
}

#[derive(Debug, thiserror::Error)]
#[error("Codex app-server {method} failed (RPC error {code})")]
pub(crate) struct RpcError {
    method: String,
    pub(crate) code: i64,
}

fn parse_reply(line: &str, id: u64, method: &str) -> anyhow::Result<Option<Value>> {
    let mut message: Value = serde_json::from_str(line).context("invalid Codex app-server JSON")?;
    if message.get("method").is_some() || message["id"].as_u64() != Some(id) {
        // Notifications and unrelated messages do not answer this request.
        return Ok(None);
    }
    if let Some(error) = message.get("error").filter(|error| !error.is_null()) {
        // Report the code only. A server error may embed private config values.
        let code = error["code"].as_i64().unwrap_or_default();
        return Err(RpcError { method: method.into(), code }.into());
    }
    let result = message.get_mut("result").with_context(|| format!("missing result for {method}"))?;
    Ok(Some(result.take()))
}

pub(crate) struct Client {
    // Close the Job Object before dropping the process and pipes so npm's cmd shim cannot
    // keep the actual server running after the request finishes or times out.
    #[cfg(windows)]
    _job: windows::KillOnClose,
    _child: Child,
    tx: ChildStdin,
    rx: FramedRead<ChildStdout, LinesCodec>,
    next_id: u64,
}

impl Client {
    pub(crate) async fn connect(mut command: Command, home: &Path) -> anyhow::Result<Self> {
        command.env("CODEX_HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .context("start codex app-server")?;
        #[cfg(windows)]
        let job = windows::KillOnClose::attach(&child)?;
        let tx = child.stdin.take().context("Codex stdin unavailable")?;
        let stdout = child.stdout.take().context("Codex stdout unavailable")?;
        let mut client = Self {
            #[cfg(windows)]
            _job: job,
            _child: child,
            tx,
            rx: FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_LINE_BYTES)),
            next_id: 1,
        };
        let _: Value = client.call("initialize", initialize_params()).await?;
        client.send(initialized()).await?;
        Ok(client)
    }

    async fn send(&mut self, message: Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(&message)?;
        line.push(b'\n');
        self.tx.write_all(&line).await.context("write to Codex app-server")
    }

    pub(crate) async fn call<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(request(id, method, params)).await?;
        while let Some(line) = self.rx.next().await {
            let line = line.context("read from Codex app-server")?;
            if let Some(result) = parse_reply(&line, id, method)? {
                return serde_json::from_value(result)
                    .with_context(|| format!("invalid response to Codex app-server {method}"));
            }
        }
        bail!("Codex app-server closed while waiting for {method}")
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use std::ptr::{null, null_mut};

    use anyhow::Context as _;
    use tokio::process::Child;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };

    pub(super) struct KillOnClose {
        _handle: OwnedHandle,
    }

    impl KillOnClose {
        pub(super) fn attach(child: &Child) -> anyhow::Result<Self> {
            let child = child.raw_handle().context("Codex process handle unavailable")?;
            // SAFETY: null arguments create a private job with default security.
            let job = unsafe { CreateJobObjectW(null(), null()) };
            if job == null_mut() {
                return Err(io::Error::last_os_error()).context("create Codex process job");
            }
            // SAFETY: CreateJobObjectW returned a new, uniquely owned handle.
            let handle = unsafe { OwnedHandle::from_raw_handle(job) };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: the handle and limits struct are valid for these calls.
            unsafe {
                if SetInformationJobObject(
                    handle.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    return Err(io::Error::last_os_error()).context("set Codex process job limits");
                }
                if AssignProcessToJobObject(handle.as_raw_handle(), child) == 0 {
                    return Err(io::Error::last_os_error()).context("attach Codex process job");
                }
            }
            Ok(Self { _handle: handle })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn initialization_is_sich_and_uses_a_notification() {
        assert_eq!(
            request(1, "initialize", initialize_params()),
            json!({
                "id": 1,
                "method": "initialize",
                "params": { "clientInfo": { "name": "sich", "version": env!("CARGO_PKG_VERSION") } }
            })
        );
        assert_eq!(initialized(), json!({ "method": "initialized" }));
    }

    #[test]
    fn reply_parser_correlates_ids_and_ignores_notifications() {
        assert_eq!(parse_reply(r#"{"id":19,"result":{}}"#, 2, "hooks/list").unwrap(), None);
        assert_eq!(parse_reply(r#"{"id":2,"method":"message","params":{}}"#, 2, "hooks/list").unwrap(), None);
        assert_eq!(
            parse_reply(r#"{"id":2,"result":{"data":[]},"extra":true}"#, 2, "hooks/list").unwrap(),
            Some(json!({ "data": [] }))
        );
        assert!(parse_reply(r#"{"id":2}"#, 2, "hooks/list").is_err());
        assert!(parse_reply("not json", 2, "hooks/list").is_err());
    }

    #[cfg(windows)]
    const FAKE_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$inputLine = [Console]::ReadLine() | ConvertFrom-Json
if ($inputLine.method -ne 'initialize' -or $inputLine.params.clientInfo.name -ne 'sich') { exit 2 }
[Console]::WriteLine('{"id":1,"result":{"userAgent":"fake"}}')
$inputLine = [Console]::ReadLine() | ConvertFrom-Json
if ($inputLine.method -ne 'initialized' -or $null -ne $inputLine.id) { exit 3 }
$inputLine = [Console]::ReadLine() | ConvertFrom-Json
if ($inputLine.id -ne 2 -or $inputLine.method -ne $env:SICH_FAKE_CODEX_METHOD) { exit 4 }
switch ($env:SICH_FAKE_CODEX_REPLY) {
    'ok' {
        [Console]::WriteLine('{"method":"warning","params":{"message":"fake warning"}}')
        [Console]::WriteLine($env:SICH_FAKE_CODEX_RESULT)
    }
    'error' {
        [Console]::WriteLine('{"id":2,"error":{"code":-32601,"message":"private data"}}')
    }
    'hang' {
        [IO.File]::WriteAllText((Join-Path $env:CODEX_HOME 'waiting'), '')
        [Console]::Write('{"id":2,')
        [Console]::Out.Flush()
        Start-Sleep -Milliseconds ([int]$env:SICH_FAKE_CODEX_HANG_MS)
        [IO.File]::WriteAllText((Join-Path $env:CODEX_HOME 'survived'), '')
    }
}
"#;

    #[cfg(unix)]
    const FAKE_SCRIPT: &str = r#"
IFS= read -r request || exit 1
case "$request" in *'"method":"initialize"'*'"name":"sich"'*) ;; *) exit 2;; esac
printf '%s\n' '{"id":1,"result":{"userAgent":"fake"}}'
IFS= read -r request || exit 1
[ "$request" = '{"method":"initialized"}' ] || exit 3
IFS= read -r request || exit 1
case "$request" in '{"id":2,"method":"'${SICH_FAKE_CODEX_METHOD}'",'* ) ;; *) exit 4;; esac
case "$SICH_FAKE_CODEX_REPLY" in
    ok)
        printf '%s\n' '{"method":"warning","params":{"message":"fake warning"}}'
        printf '%s\n' "$SICH_FAKE_CODEX_RESULT"
        ;;
    error) printf '%s\n' '{"id":2,"error":{"code":-32601,"message":"private data"}}';;
    hang)
        : > "$CODEX_HOME/waiting"
        printf '%s' '{"id":2,'
        sleep "$SICH_FAKE_CODEX_HANG_SECONDS"
        : > "$CODEX_HOME/survived"
        ;;
esac
"#;

    fn fake_command(dir: &Path, reply: &str) -> Command {
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt as _;
            let script = dir.join("fake-server.ps1");
            fs::write(&script, FAKE_SCRIPT).unwrap();
            let mut command = Command::new("powershell.exe");
            command
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(script)
                .creation_flags(0x0800_0000);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let script = dir.join("fake-server.sh");
            fs::write(&script, FAKE_SCRIPT).unwrap();
            let mut command = Command::new("sh");
            command.arg(script);
            command
        };
        command.env("SICH_FAKE_CODEX_REPLY", reply)
            .env("SICH_FAKE_CODEX_METHOD", "hooks/list")
            .env("SICH_FAKE_CODEX_HANG_MS", "850")
            .env("SICH_FAKE_CODEX_HANG_SECONDS", "0.85")
            .env("SICH_FAKE_CODEX_RESULT", r#"{"id":2,"result":{"data":[]}}"#);
        command
    }

    #[test]
    fn fake_process_handshake_and_list() {
        let home = tempfile::tempdir().unwrap();
        let result: Value = bounded(async {
            let mut client = Client::connect(fake_command(home.path(), "ok"), home.path()).await?;
            client.call("hooks/list", json!({ "cwds": [home.path()] })).await
        })
        .unwrap();
        assert_eq!(result, json!({ "data": [] }));
    }

    #[test]
    fn fake_process_rpc_error() {
        let home = tempfile::tempdir().unwrap();
        let result: anyhow::Result<Value> = bounded(async {
            let mut client = Client::connect(fake_command(home.path(), "error"), home.path()).await?;
            client.call("hooks/list", json!({ "cwds": [home.path()] })).await
        });
        assert_eq!(result.unwrap_err().to_string(), "Codex app-server hooks/list failed (RPC error -32601)");
    }

    #[test]
    fn timeout_kills_fake_process_even_with_a_partial_line() {
        let home = tempfile::tempdir().unwrap();
        bounded(async {
            let mut client = Client::connect(fake_command(home.path(), "hang"), home.path()).await?;
            let result = tokio::time::timeout(Duration::from_millis(350), async move {
                client.call::<Value>("hooks/list", json!({ "cwds": [] })).await
            })
            .await;
            assert!(result.is_err());
            assert!(home.path().join("waiting").exists());
            tokio::time::sleep(Duration::from_millis(900)).await;
            assert!(!home.path().join("survived").exists());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn fake_usage_success_error_and_timeout() {
        use crate::usage::query_codex;

        let home = tempfile::tempdir().unwrap();
        let fake = |reply| {
            let mut command = fake_command(home.path(), reply);
            command.env("SICH_FAKE_CODEX_METHOD", "account/rateLimits/read")
                .env("SICH_FAKE_CODEX_RESULT", r#"{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":84}}}}"#)
                .env("SICH_FAKE_CODEX_HANG_MS", "4000")
                .env("SICH_FAKE_CODEX_HANG_SECONDS", "4");
            command
        };
        let windows = bounded(query_codex(fake("ok"), home.path())).unwrap();
        assert_eq!(windows.five_hour.unwrap().used_percent, 84.0);
        let error = bounded(query_codex(fake("error"), home.path())).unwrap_err();
        assert_eq!(error.downcast_ref::<RpcError>().unwrap().code, -32601);
        assert!(!error.to_string().contains("private data"));
        bounded(async {
            let result = tokio::time::timeout(
                Duration::from_secs(2),
                query_codex(fake("hang"), home.path()),
            ).await;
            assert!(result.is_err());
            assert!(home.path().join("waiting").exists());
            Ok(())
        }).unwrap();
    }
}
