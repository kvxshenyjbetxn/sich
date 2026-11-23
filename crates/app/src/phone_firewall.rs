//! Windows Firewall rule for the phone gateway's fixed UDP port.
//!
//! Two rules: one by port, and one by program path for the gateway's stable copy
//! (`gateway-host/sich.exe`); Windows asks "allow sich.exe" once per program path and only a
//! program rule silences that. Status is read without elevation (`netsh`); adding/removing runs one elevated,
//! hidden PowerShell (a single UAC prompt). Command building and result parsing are pure and
//! unit-tested; nothing here is run by tests.

use std::os::windows::process::CommandExt as _;
use std::process::{Command, Stdio};

use base64::Engine as _;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `ERROR_CANCELLED`: the user dismissed the UAC prompt.
pub const EXIT_CANCELLED: i32 = 1223;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Add,
    Remove,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done,
    Cancelled,
    Failed(String),
}

/// Port the rule covers: `SICH_GATEWAY_PORT`, or the default when unset or `0` (random).
pub fn rule_port() -> u16 {
    match sich_gateway::port_from_env(std::env::var("SICH_GATEWAY_PORT").ok().as_deref()) {
        0 => sich_gateway::DEFAULT_PORT,
        port => port,
    }
}

pub fn rule_name(port: u16) -> String {
    format!("Sich phone sync (UDP {port})")
}

/// Display name of the program rule for the stable gateway executable.
pub const PROGRAM_RULE_NAME: &str = "Sich phone gateway";

/// Arguments of the unelevated status query for the rule called `name`.
pub fn show_named_rule_args(name: &str) -> Vec<String> {
    ["advfirewall", "firewall", "show", "rule"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(format!("name={name}")))
        .collect()
}

/// Allowed only when both the port rule and the program rule exist.
pub fn all_present(port_rule: bool, program_rule: bool) -> bool {
    port_rule && program_rule
}

/// Body of a PowerShell single-quoted string: quotes doubled.
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// `netsh ... show rule` exits 0 when a rule matches and 1 ("No rules match") otherwise; the
/// text is localized, so only the exit code counts.
pub fn rule_present(exit_code: Option<i32>) -> bool {
    exit_code == Some(0)
}

/// The script the elevated PowerShell runs. Idempotent: `Add` replaces existing rules. Both
/// rules are handled in one script (one UAC prompt); `program` is the stable gateway exe.
pub fn inner_script(action: Action, port: u16, program: &str) -> String {
    let name = rule_name(port);
    let prog_name = PROGRAM_RULE_NAME;
    let program = ps_quote(program);
    let remove = format!(
        "Remove-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue; \
         Remove-NetFirewallRule -DisplayName '{prog_name}' -ErrorAction SilentlyContinue"
    );
    let body = match action {
        Action::Add => format!(
            "{remove}; \
             New-NetFirewallRule -DisplayName '{name}' -Direction Inbound -Protocol UDP -LocalPort {port} \
             -Action Allow -Profile Any -ErrorAction Stop | Out-Null; \
             New-NetFirewallRule -DisplayName '{prog_name}' -Direction Inbound -Protocol UDP \
             -Program '{program}' -Action Allow -Profile Any -ErrorAction Stop | Out-Null"
        ),
        Action::Remove => remove,
    };
    format!("try {{ {body} }} catch {{ exit 1 }}")
}

/// PowerShell `-EncodedCommand` value: base64 of the UTF-16LE text.
pub fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The unelevated wrapper: starts the inner script elevated and hidden, waits, and passes its
/// exit code on. A dismissed UAC prompt (Win32 error 1223) becomes exit code 1223.
pub fn outer_script(action: Action, port: u16, program: &str) -> String {
    let inner = encode_command(&inner_script(action, port, program));
    format!(
        "try {{ $p = Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru \
         -ArgumentList '-NoProfile','-NonInteractive','-EncodedCommand','{inner}'; exit $p.ExitCode }} \
         catch {{ $e = $_.Exception; \
         if ($e.NativeErrorCode -eq {EXIT_CANCELLED} -or $e.InnerException.NativeErrorCode -eq {EXIT_CANCELLED}) \
         {{ exit {EXIT_CANCELLED} }} else {{ exit 1 }} }}"
    )
}

pub fn powershell_args(action: Action, port: u16, program: &str) -> Vec<String> {
    vec![
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-WindowStyle".into(),
        "Hidden".into(),
        "-EncodedCommand".into(),
        encode_command(&outer_script(action, port, program)),
    ]
}

pub fn outcome_from_exit(code: Option<i32>) -> Outcome {
    match code {
        Some(0) => Outcome::Done,
        Some(EXIT_CANCELLED) => Outcome::Cancelled,
        Some(code) => Outcome::Failed(format!("exit code {code}")),
        None => Outcome::Failed("terminated".into()),
    }
}

/// Are both rules present? Blocking (spawns `netsh`); any failure counts as absent.
pub fn is_present(port: u16) -> bool {
    let port_rule = named_rule_present(&rule_name(port));
    all_present(port_rule, port_rule && named_rule_present(PROGRAM_RULE_NAME))
}

fn named_rule_present(name: &str) -> bool {
    let status = Command::new("netsh")
        .args(show_named_rule_args(name))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    rule_present(status.ok().and_then(|s| s.code()))
}

/// Adds or removes the rule through one UAC prompt. Blocking until the prompt is answered.
pub fn run_elevated(action: Action, port: u16) -> Outcome {
    let Some(program) = crate::gateway_host::stable_exe_path() else {
        return Outcome::Failed("no local data dir".into());
    };
    let status = Command::new("powershell.exe")
        .args(powershell_args(action, port, &program.to_string_lossy()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    match status {
        Ok(status) => outcome_from_exit(status.code()),
        Err(e) => Outcome::Failed(format!("{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROG: &str = r"C:\Data\sich\gateway-host\sich.exe";

    #[test]
    fn program_rule_uses_the_stable_path_and_both_rules_are_removed() {
        let add = inner_script(Action::Add, 6769, PROG);
        assert!(add.contains(r"-Program 'C:\Data\sich\gateway-host\sich.exe'"));
        let create = add.find("New-NetFirewallRule -DisplayName 'Sich phone gateway'").unwrap();
        let remove = add.find("Remove-NetFirewallRule -DisplayName 'Sich phone gateway'").unwrap();
        assert!(remove < create);
        assert!(add.contains("-Protocol UDP") && add.contains("-Direction Inbound"));
        let rm = inner_script(Action::Remove, 6769, PROG);
        assert!(rm.contains("Remove-NetFirewallRule -DisplayName 'Sich phone gateway'"));
        assert!(rm.contains("Remove-NetFirewallRule -DisplayName 'Sich phone sync (UDP 6769)'"));
        assert!(!rm.contains("New-NetFirewallRule"));
        // A quote in the path cannot break out of the string.
        assert!(inner_script(Action::Add, 1, r"C:\o'x\sich.exe").contains(r"-Program 'C:\o''x\sich.exe'"));
    }

    #[test]
    fn allowed_needs_both_rules() {
        assert!(all_present(true, true));
        assert!(!all_present(true, false));
        assert!(!all_present(false, true));
        assert_eq!(
            show_named_rule_args(PROGRAM_RULE_NAME),
            ["advfirewall", "firewall", "show", "rule", "name=Sich phone gateway"]
        );
    }

    #[test]
    fn rule_name_matches_the_spec() {
        assert_eq!(rule_name(6769), "Sich phone sync (UDP 6769)");
    }

    #[test]
    fn show_command_is_netsh_by_name() {
        assert_eq!(
            show_named_rule_args(&rule_name(6769)),
            ["advfirewall", "firewall", "show", "rule", "name=Sich phone sync (UDP 6769)"]
        );
    }

    #[test]
    fn presence_follows_the_exit_code_only() {
        assert!(rule_present(Some(0)));
        assert!(!rule_present(Some(1)));
        assert!(!rule_present(None));
    }

    #[test]
    fn add_script_replaces_then_creates_an_inbound_udp_allow_rule() {
        let s = inner_script(Action::Add, 6769, PROG);
        let remove = s.find("Remove-NetFirewallRule -DisplayName 'Sich phone sync (UDP 6769)'").unwrap();
        let create = s.find("New-NetFirewallRule -DisplayName 'Sich phone sync (UDP 6769)'").unwrap();
        assert!(remove < create);
        for part in ["-Direction Inbound", "-Protocol UDP", "-LocalPort 6769", "-Action Allow", "-Profile Any"] {
            assert!(s.contains(part), "{part} missing in {s}");
        }
    }

    #[test]
    fn remove_script_only_removes() {
        let s = inner_script(Action::Remove, 7000, PROG);
        assert!(s.contains("Remove-NetFirewallRule -DisplayName 'Sich phone sync (UDP 7000)'"));
        assert!(!s.contains("New-NetFirewallRule"));
    }

    #[test]
    fn encoded_command_is_utf16le_base64() {
        // "A" -> 41 00
        assert_eq!(encode_command("A"), "QQA=");
        let decoded = base64::engine::general_purpose::STANDARD.decode(encode_command("Ї-x")).unwrap();
        let units: Vec<u16> = decoded.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(String::from_utf16(&units).unwrap(), "Ї-x");
    }

    #[test]
    fn outer_script_elevates_hidden_and_reports_cancel() {
        let s = outer_script(Action::Add, 6769, PROG);
        assert!(s.contains("-Verb RunAs") && s.contains("-WindowStyle Hidden") && s.contains("-Wait"));
        assert!(s.contains(&encode_command(&inner_script(Action::Add, 6769, PROG))));
        assert!(s.contains("1223"));
        let args = powershell_args(Action::Remove, 6769, PROG);
        assert_eq!(args[4], "-EncodedCommand");
        assert_eq!(args[5], encode_command(&outer_script(Action::Remove, 6769, PROG)));
        // Encoded, so no quoting problems on the command line.
        assert!(args[5].chars().all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)));
    }

    #[test]
    fn exit_codes_map_to_outcomes() {
        assert_eq!(outcome_from_exit(Some(0)), Outcome::Done);
        assert_eq!(outcome_from_exit(Some(1223)), Outcome::Cancelled);
        assert!(matches!(outcome_from_exit(Some(1)), Outcome::Failed(_)));
        assert!(matches!(outcome_from_exit(None), Outcome::Failed(_)));
    }

    #[test]
    fn rule_port_is_never_random() {
        assert_ne!(rule_port(), 0);
    }
}
