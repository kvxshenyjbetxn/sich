//! Registers `sich hook codex` in the OpenAI Codex CLI's `hooks.json` (documented hooks mechanism).
//!
//! Safety rules: only our own entries are ever added, changed or removed; everything else (user
//! hooks, key order) is kept; an unparsable `hooks.json` is never touched; the first modification
//! makes a backup. We never write Codex's trust state (`[hooks.state]` / `trusted_hash`) and never
//! bypass hook trust: the user approves our hooks once with `/hooks` inside Codex.
//!
//! Codex identifies a hook for trust by `<path>:<event>:<group_index>:<handler_index>`, so our
//! matcher group is always APPENDED to an event's array (user hook indexes never shift) and an
//! existing entry of ours is replaced in place. With a stable command and timeout the approval
//! persists across re-installs.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value, json};

const HOOKS_FILE: &str = "hooks.json";
const BACKUP_FILE: &str = "hooks.json.sich-backup";
const TMP_FILE: &str = "hooks.json.sich-tmp";
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

type Version = (u32, u32, u32);

/// Hooks need at least this Codex version.
pub const MIN_CODEX_VERSION: Version = (0, 129, 0);

/// A Codex hook event we register.
#[derive(Debug, Clone, Copy)]
pub struct EventDef {
    pub name: &'static str,
    pub min_version: Version,
    pub timeout_secs: u64,
}

const fn ev(name: &'static str, min_version: Version, timeout_secs: u64) -> EventDef {
    EventDef { name, min_version, timeout_secs }
}

/// `SubagentStart`/`SubagentStop` exist since 0.133. `Interrupt` is documented for >= 0.150; for `SessionEnd` no minimum could be confirmed from the
/// docs, so it is gated at 0.150 as well.
pub const EVENTS: &[EventDef] = &[
    ev("SessionStart", MIN_CODEX_VERSION, 5),
    ev("UserPromptSubmit", MIN_CODEX_VERSION, 5),
    ev("PreToolUse", MIN_CODEX_VERSION, 5),
    ev("PermissionRequest", MIN_CODEX_VERSION, 5),
    ev("PostToolUse", MIN_CODEX_VERSION, 5),
    ev("Stop", MIN_CODEX_VERSION, 5),
    ev("Interrupt", (0, 150, 0), 3),
    ev("SessionEnd", (0, 150, 0), 3),
    ev("SubagentStart", (0, 133, 0), 5),
    ev("SubagentStop", (0, 133, 0), 5),
];

/// Events that must carry our exact command for `is_installed`.
const CORE_EVENTS: &[&str] =
    &["SessionStart", "UserPromptSubmit", "PreToolUse", "PermissionRequest", "PostToolUse", "Stop"];

/// Why nothing was installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// `codex --version` failed or was not recognised.
    NotFound,
    /// Codex is older than [`MIN_CODEX_VERSION`].
    TooOld,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallReport {
    pub hooks_path: PathBuf,
    pub codex_version: Option<String>,
    /// Events that now have our entry.
    pub installed: Vec<String>,
    /// Events whose entry of ours was removed.
    pub removed: Vec<String>,
    /// False when `hooks.json` already was in the desired state (nothing was written).
    pub changed: bool,
    pub backup_created: bool,
    /// Set when the version gate stopped the install (the file was not touched).
    pub skipped: Option<Skipped>,
}

/// `CODEX_HOME` if set, else `~/.codex`.
pub fn default_codex_home() -> Option<PathBuf> {
    codex_home_from(std::env::var_os("CODEX_HOME"), dirs::home_dir())
}

fn codex_home_from(env: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match env.filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home.map(|h| h.join(".codex")),
    }
}

/// The command written into `hooks.json`: `<exe> hook codex` with forward slashes; the path is
/// double-quoted only when it contains spaces.
pub fn hook_command(exe: &Path) -> String {
    let mut p = exe.display().to_string().replace('\\', "/");
    if let Some(rest) = p.strip_prefix("//?/") {
        p = rest.to_string();
    }
    if p.contains(' ') { format!("\"{p}\" hook codex") } else { format!("{p} hook codex") }
}

// ---------------------------------------------------------------------------------------------
// Version handling

fn parse_version_triple(s: &str) -> Option<Version> {
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    let c = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((a, b, c))
}

/// Extracts `x.y.z` from `codex --version` output (`codex-cli 0.170.0`).
pub fn parse_codex_version_output(output: &str) -> Option<String> {
    for line in output.lines() {
        if !line.to_ascii_lowercase().contains("codex") {
            continue;
        }
        for token in line.split_whitespace() {
            let token = token.trim_start_matches('v');
            if parse_version_triple(token).is_some() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// The events to install for `version` (`None`/unparsable or older than 0.129: none).
pub fn events_for_version(version: Option<&str>) -> Vec<&'static EventDef> {
    match version.and_then(parse_version_triple) {
        Some(v) if v >= MIN_CODEX_VERSION => EVENTS.iter().filter(|e| e.min_version <= v).collect(),
        _ => Vec::new(),
    }
}

/// Runs `codex --version` (hidden window, 3 s timeout).
pub fn probe_codex_version() -> Option<String> {
    let mut cmd;
    if cfg!(windows) {
        // `codex` may be a `.cmd` shim (npm install), which only the shell resolves.
        cmd = Command::new("cmd");
        cmd.args(["/C", "codex", "--version"]);
    } else {
        cmd = Command::new("codex");
        cmd.arg("--version");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + VERSION_PROBE_TIMEOUT;
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                return parse_codex_version_output(&text);
            }
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Recognising our entries

/// Splits a command line into words; double and single quotes group, no escapes.
fn tokenize(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

fn program_stem(program: &str) -> String {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").map(str::to_string).unwrap_or(lower)
}

/// True for commands whose program file stem is `sich` and whose args are `hook codex`.
pub fn is_our_command(cmd: &str) -> bool {
    let t = tokenize(cmd);
    t.len() == 3 && program_stem(&t[0]) == "sich" && t[1] == "hook" && t[2] == "codex"
}

fn is_our_hook(h: &Value) -> bool {
    h.get("command").and_then(Value::as_str).is_some_and(is_our_command)
}

// ---------------------------------------------------------------------------------------------
// hooks.json merging

fn desired_group(command: &str, timeout_secs: u64) -> Value {
    json!({ "hooks": [ { "type": "command", "command": command, "timeout": timeout_secs } ] })
}

fn desired_hook(command: &str, timeout_secs: u64) -> Value {
    json!({ "type": "command", "command": command, "timeout": timeout_secs })
}

/// Strips our entries from one event's matcher groups. With `replace_with`, the first one is
/// replaced in place (and the rest removed); returns (placed, removed_any). Groups that become
/// empty because of us are dropped.
fn strip_ours(groups: &mut Vec<Value>, replace_with: Option<&Value>) -> (bool, bool) {
    let mut placed = false;
    let mut removed_any = false;
    let mut emptied: Vec<bool> = Vec::with_capacity(groups.len());
    for g in groups.iter_mut() {
        let mut emptied_by_us = false;
        if let Some(hooks) = g.get_mut("hooks").and_then(Value::as_array_mut) {
            let had_ours = hooks.iter().any(is_our_hook);
            let mut k = 0;
            while k < hooks.len() {
                if is_our_hook(&hooks[k]) {
                    match replace_with {
                        Some(new) if !placed => {
                            if hooks[k] != *new {
                                hooks[k] = new.clone();
                            }
                            placed = true;
                            k += 1;
                        }
                        _ => {
                            hooks.remove(k);
                            removed_any = true;
                        }
                    }
                } else {
                    k += 1;
                }
            }
            emptied_by_us = had_ours && hooks.is_empty();
        }
        emptied.push(emptied_by_us);
    }
    let mut it = emptied.into_iter();
    groups.retain(|_| !it.next().unwrap_or(false));
    (placed, removed_any)
}

fn hooks_object(root: &mut Value, create: bool) -> anyhow::Result<Option<&mut Map<String, Value>>> {
    let Some(obj) = root.as_object_mut() else {
        bail!("hooks.json is not a JSON object");
    };
    if !obj.contains_key("hooks") {
        if !create {
            return Ok(None);
        }
        obj.insert("hooks".into(), Value::Object(Map::new()));
    }
    match obj.get_mut("hooks") {
        Some(Value::Object(h)) => Ok(Some(h)),
        _ => bail!("`hooks` in hooks.json is not an object"),
    }
}

/// Removes our entries from the events named in `events` (all events when `None`), dropping
/// arrays that we emptied. Returns the events that lost an entry.
fn remove_ours(hooks: &mut Map<String, Value>, events: Option<&[&str]>) -> anyhow::Result<Vec<String>> {
    let names: Vec<String> = match events {
        Some(e) => e.iter().map(|s| s.to_string()).collect(),
        None => hooks.keys().cloned().collect(),
    };
    let mut removed = Vec::new();
    for name in names {
        let Some(slot) = hooks.get_mut(&name) else { continue };
        let Some(groups) = slot.as_array_mut() else {
            if events.is_some() {
                bail!("hooks.{name} in hooks.json is not an array");
            }
            continue;
        };
        let (_, removed_any) = strip_ours(groups, None);
        if removed_any {
            removed.push(name.clone());
            if groups.is_empty() {
                hooks.shift_remove(&name);
            }
        }
    }
    Ok(removed)
}

fn merge_install(
    root: &mut Value,
    command: &str,
    plan: &[&EventDef],
    retire: &[&str],
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let hooks = hooks_object(root, true)?.expect("created");
    let removed = remove_ours(hooks, Some(retire))?;
    let mut installed = Vec::new();
    for def in plan {
        let slot = hooks.entry(def.name).or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = slot.as_array_mut() else {
            bail!("hooks.{} in hooks.json is not an array", def.name);
        };
        let (placed, _) = strip_ours(groups, Some(&desired_hook(command, def.timeout_secs)));
        if !placed {
            // Appended at the END: existing user hook indexes (Codex trust keys) never shift.
            groups.push(desired_group(command, def.timeout_secs));
        }
        installed.push(def.name.to_string());
    }
    Ok((installed, removed))
}

// ---------------------------------------------------------------------------------------------
// File handling

fn hooks_path(dir: &Path) -> PathBuf {
    dir.join(HOOKS_FILE)
}

/// Reads hooks.json: `Ok(None)` when missing, an empty file counts as `{}`.
fn read_hooks(path: &Path) -> anyhow::Result<Option<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some(Value::Object(Map::new())));
    }
    let v = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .with_context(|| format!("{} is not valid JSON; left untouched", path.display()))?;
    Ok(Some(v))
}

/// Writes atomically (temp file + rename), making the one-time backup first.
fn write_hooks(dir: &Path, root: &Value, existed: bool) -> anyhow::Result<bool> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = hooks_path(dir);
    let mut backup_created = false;
    if existed {
        let backup = dir.join(BACKUP_FILE);
        if !backup.exists() {
            std::fs::copy(&path, &backup).with_context(|| format!("backup {}", path.display()))?;
            backup_created = true;
        }
    }
    let mut text = serde_json::to_string_pretty(root)?;
    text.push('\n');
    let tmp = dir.join(TMP_FILE);
    std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replace {}", path.display()));
    }
    Ok(backup_created)
}

/// Installs for the given Codex version (`None` = `codex --version` failed). Below 0.129 or with
/// an unknown version nothing is written and `report.skipped` says why. Use
/// [`probe_codex_version`] to get the version.
pub fn install(codex_home: &Path, exe: &Path, version: Option<&str>) -> anyhow::Result<InstallReport> {
    let path = hooks_path(codex_home);
    let mut report = InstallReport {
        hooks_path: path.clone(),
        codex_version: version.map(str::to_string),
        ..Default::default()
    };
    let plan = events_for_version(version);
    if plan.is_empty() {
        report.skipped =
            Some(if version.and_then(parse_version_triple).is_some() { Skipped::TooOld } else { Skipped::NotFound });
        return Ok(report);
    }
    let original = read_hooks(&path)?;
    let existed = original.is_some();
    let mut root = original.clone().unwrap_or_else(|| Value::Object(Map::new()));

    // Known version: events it does not support are retired (a downgrade).
    let retire: Vec<&str> =
        EVENTS.iter().filter(|e| !plan.iter().any(|p| p.name == e.name)).map(|e| e.name).collect();
    let (installed, removed) = merge_install(&mut root, &hook_command(exe), &plan, &retire)?;
    report.installed = installed;
    report.removed = removed;
    if original.as_ref() != Some(&root) {
        report.backup_created = write_hooks(codex_home, &root, existed)?;
        report.changed = true;
    }
    Ok(report)
}

/// Removes our entries only. `exe` is accepted for symmetry: entries are recognised by shape, so
/// stale paths are removed as well.
pub fn uninstall(codex_home: &Path, _exe: &Path) -> anyhow::Result<InstallReport> {
    let path = hooks_path(codex_home);
    let mut report = InstallReport { hooks_path: path.clone(), ..Default::default() };
    let Some(original) = read_hooks(&path)? else { return Ok(report) };
    let mut root = original.clone();
    if let Some(hooks) = hooks_object(&mut root, false)? {
        report.removed = remove_ours(hooks, None)?;
        if hooks.is_empty() && !report.removed.is_empty() {
            root.as_object_mut().expect("object").shift_remove("hooks");
        }
    }
    if root != original {
        report.backup_created = write_hooks(codex_home, &root, true)?;
        report.changed = true;
    }
    Ok(report)
}

/// True if the core events carry exactly the hook command for `exe` (a stale path counts as not
/// installed, so the UI offers a reinstall).
pub fn is_installed(codex_home: &Path, exe: &Path) -> bool {
    let Ok(Some(root)) = read_hooks(&hooks_path(codex_home)) else { return false };
    let command = hook_command(exe);
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else { return false };
    CORE_EVENTS.iter().all(|name| {
        hooks.get(*name).and_then(Value::as_array).is_some_and(|groups| {
            groups.iter().any(|g| {
                g.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
                    hs.iter().any(|h| h.get("command").and_then(Value::as_str) == Some(command.as_str()))
                })
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const EXE: &str = r"C:\Apps\Sich\sich.exe";

    fn exe() -> PathBuf {
        PathBuf::from(EXE)
    }

    fn read(dir: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(dir.join(HOOKS_FILE)).unwrap()).unwrap()
    }

    fn write(dir: &Path, text: &str) {
        fs::write(dir.join(HOOKS_FILE), text).unwrap();
    }

    fn ours_count(v: &Value) -> usize {
        let mut n = 0;
        if let Some(hooks) = v.get("hooks").and_then(Value::as_object) {
            for groups in hooks.values() {
                for g in groups.as_array().unwrap() {
                    for h in g["hooks"].as_array().unwrap() {
                        if is_our_command(h["command"].as_str().unwrap()) {
                            n += 1;
                        }
                    }
                }
            }
        }
        n
    }

    const USER: &str = r#"{
  "zeta": 1,
  "hooks": {
    "Stop": [
      { "hooks": [ { "type": "command", "command": "echo user-stop" } ] }
    ],
    "PreToolUse": [
      { "matcher": "shell", "hooks": [ { "type": "command", "command": "echo a", "timeout": 10 } ] },
      { "hooks": [ { "type": "command", "command": "echo b" } ] }
    ],
    "CustomUserEvent": [ { "hooks": [] } ]
  },
  "alpha": [1, 2]
}"#;

    #[test]
    fn command_format() {
        assert_eq!(
            hook_command(Path::new(r"E:\dev\program\sich\target\release\sich.exe")),
            "E:/dev/program/sich/target/release/sich.exe hook codex"
        );
        assert_eq!(hook_command(Path::new(r"C:\Program Files\Sich\sich.exe")), "\"C:/Program Files/Sich/sich.exe\" hook codex");
        assert_eq!(hook_command(Path::new(r"\\?\C:\a\sich.exe")), "C:/a/sich.exe hook codex");
        assert!(is_our_command(&hook_command(Path::new(r"C:\Program Files\Sich\sich.exe"))));
        assert!(is_our_command(&hook_command(&exe())));
    }

    #[test]
    fn recognises_our_commands() {
        assert!(is_our_command("E:/x/sich.exe hook codex"));
        assert!(is_our_command(r#""C:\a b\sich.exe" hook codex"#));
        assert!(is_our_command("sich hook codex"));
        assert!(!is_our_command("E:/x/sich.exe hook claude"));
        assert!(!is_our_command("E:/x/other.exe hook codex"));
        assert!(!is_our_command("E:/x/sich.exe hook codex --x"));
        assert!(!is_our_command(""));
    }

    #[test]
    fn codex_home_resolution() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(codex_home_from(Some("/x/c".into()), home.clone()), Some(PathBuf::from("/x/c")));
        assert_eq!(codex_home_from(None, home.clone()), Some(PathBuf::from("/home/u/.codex")));
        assert_eq!(codex_home_from(Some("".into()), home), Some(PathBuf::from("/home/u/.codex")));
        assert_eq!(codex_home_from(None, None), None);
    }

    #[test]
    fn version_output_parsing() {
        assert_eq!(parse_codex_version_output("codex-cli 0.170.0\n"), Some("0.170.0".into()));
        assert_eq!(parse_codex_version_output("warn\ncodex-cli 0.129.1"), Some("0.129.1".into()));
        assert_eq!(parse_codex_version_output("codex 1.0.0-beta.1"), Some("1.0.0-beta.1".into()));
        assert_eq!(parse_codex_version_output("npx 10.2.0"), None);
        assert_eq!(parse_codex_version_output("codex-cli"), None);
        assert_eq!(parse_codex_version_output(""), None);
    }

    fn names(v: Option<&str>) -> Vec<&'static str> {
        events_for_version(v).iter().map(|e| e.name).collect()
    }

    #[test]
    fn version_gating() {
        assert!(names(Some("0.128.9")).is_empty());
        assert!(names(None).is_empty());
        assert!(names(Some("garbage")).is_empty());
        let v129 = names(Some("0.129.0"));
        assert_eq!(v129.len(), 6);
        assert!(!v129.contains(&"Interrupt") && !v129.contains(&"SessionEnd"));
        assert!(!v129.contains(&"SubagentStart") && !v129.contains(&"SubagentStop"));
        let v132 = names(Some("0.132.9"));
        assert!(!v132.contains(&"SubagentStart") && !v132.contains(&"SubagentStop"));
        let v133 = names(Some("0.133.0"));
        assert_eq!(v133.len(), 8);
        assert!(v133.contains(&"SubagentStart") && v133.contains(&"SubagentStop"));
        assert!(!v133.contains(&"Interrupt") && !v133.contains(&"SessionEnd"));
        assert!(!names(Some("0.149.9")).contains(&"Interrupt"));
        assert_eq!(names(Some("0.150.0")).len(), EVENTS.len());
        assert_eq!(names(Some("0.170.0")).len(), EVENTS.len());
        assert_eq!(names(Some("1.0.0")).len(), EVENTS.len());
    }

    #[test]
    fn gated_install_does_not_touch_anything() {
        for (v, why) in [(None, Skipped::NotFound), (Some("0.128.0"), Skipped::TooOld)] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), USER);
            let r = install(dir.path(), &exe(), v).unwrap();
            assert_eq!(r.skipped, Some(why));
            assert!(!r.changed && r.installed.is_empty());
            assert_eq!(fs::read_to_string(dir.path().join(HOOKS_FILE)).unwrap(), USER);
            assert!(!dir.path().join(BACKUP_FILE).exists());
            // And no file is created for a missing hooks.json.
            let empty = tempfile::tempdir().unwrap();
            install(empty.path(), &exe(), v).unwrap();
            assert!(!empty.path().join(HOOKS_FILE).exists());
        }
    }

    #[test]
    fn fresh_install_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("nested").join(".codex");
        let r = install(&home, &exe(), Some("0.170.0")).unwrap();
        assert!(r.changed && !r.backup_created && r.skipped.is_none());
        assert!(!home.join(BACKUP_FILE).exists() && !home.join(TMP_FILE).exists());
        let v = read(&home);
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), EVENTS.len());
        let entry = &hooks["Stop"][0]["hooks"][0];
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["command"], hook_command(&exe()));
        assert_eq!(entry["timeout"], 5);
        assert!(hooks["Stop"][0].get("matcher").is_none());
        assert!(entry.get("statusMessage").is_none());
        assert_eq!(hooks["Interrupt"][0]["hooks"][0]["timeout"], 3);
        assert_eq!(hooks["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        assert!(is_installed(&home, &exe()));
    }

    #[test]
    fn install_0129_has_no_new_events() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), &exe(), Some("0.129.0")).unwrap();
        let v = read(dir.path());
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 6);
        assert!(hooks.get("Interrupt").is_none() && hooks.get("SessionEnd").is_none());
        assert!(hooks.get("SubagentStart").is_none() && hooks.get("SubagentStop").is_none());
    }

    #[test]
    fn install_0133_has_subagent_events_and_uninstall_removes_them() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), &exe(), Some("0.133.0")).unwrap();
        let v = read(dir.path());
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 8);
        assert_eq!(hooks["SubagentStart"][0]["hooks"][0]["timeout"], 5);
        assert_eq!(hooks["SubagentStop"][0]["hooks"][0]["command"], hook_command(&exe()));
        assert!(hooks.get("Interrupt").is_none());
        let r = uninstall(dir.path(), &exe()).unwrap();
        assert!(r.removed.contains(&"SubagentStart".to_string()) && r.removed.contains(&"SubagentStop".to_string()));
        assert_eq!(read(dir.path()), json!({}));
    }

    #[test]
    fn append_at_end_preserves_user_indexes_and_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER);
        let before: Value = serde_json::from_str(USER).unwrap();
        install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        let after = read(dir.path());
        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "hooks", "alpha"]);
        let hook_keys: Vec<&String> = after["hooks"].as_object().unwrap().keys().collect();
        assert_eq!(&hook_keys[..3], ["Stop", "PreToolUse", "CustomUserEvent"]);
        assert_eq!(after["hooks"]["Stop"][0], before["hooks"]["Stop"][0]);
        assert_eq!(after["hooks"]["PreToolUse"][0], before["hooks"]["PreToolUse"][0]);
        assert_eq!(after["hooks"]["PreToolUse"][1], before["hooks"]["PreToolUse"][1]);
        assert_eq!(after["hooks"]["CustomUserEvent"], before["hooks"]["CustomUserEvent"]);
        // Ours is last.
        assert_eq!(after["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(after["hooks"]["PreToolUse"].as_array().unwrap().len(), 3);
        assert!(is_our_command(after["hooks"]["PreToolUse"][2]["hooks"][0]["command"].as_str().unwrap()));
        assert_eq!(after["alpha"], before["alpha"]);
        assert_eq!(ours_count(&after), EVENTS.len());
    }

    #[test]
    fn reinstall_is_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER);
        install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        let first = fs::read_to_string(dir.path().join(HOOKS_FILE)).unwrap();
        let r = install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        assert!(!r.changed && !r.backup_created);
        assert_eq!(fs::read_to_string(dir.path().join(HOOKS_FILE)).unwrap(), first);
    }

    #[test]
    fn changed_exe_is_updated_in_place() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER);
        let old = PathBuf::from(r"C:\dev\sich\target\debug\sich.exe");
        install(dir.path(), &old, Some("0.170.0")).unwrap();
        let r = install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        assert!(r.changed);
        let v = read(dir.path());
        assert_eq!(ours_count(&v), EVENTS.len(), "no duplicates");
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 3);
        assert_eq!(v["hooks"]["PreToolUse"][2]["hooks"][0]["command"], hook_command(&exe()));
        assert!(is_installed(dir.path(), &exe()));
        assert!(!is_installed(dir.path(), &old));
    }

    #[test]
    fn downgrade_retires_newer_events() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        let r = install(dir.path(), &exe(), Some("0.140.0")).unwrap();
        assert!(r.removed.contains(&"Interrupt".to_string()));
        let v = read(dir.path());
        assert!(v["hooks"].get("Interrupt").is_none() && v["hooks"].get("SessionEnd").is_none());
        assert_eq!(ours_count(&v), 8);
        let r = install(dir.path(), &exe(), Some("0.130.0")).unwrap();
        assert!(r.removed.contains(&"SubagentStart".to_string()));
        assert_eq!(ours_count(&read(dir.path())), 6);
    }

    #[test]
    fn uninstall_removes_only_ours_and_tidies() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER);
        let before: Value = serde_json::from_str(USER).unwrap();
        install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        let r = uninstall(dir.path(), &exe()).unwrap();
        assert!(r.changed);
        assert_eq!(read(dir.path()), before);
        assert!(!is_installed(dir.path(), &exe()));
        assert!(!uninstall(dir.path(), &exe()).unwrap().changed);

        // Only our events: arrays and the `hooks` key disappear.
        let d2 = tempfile::tempdir().unwrap();
        write(d2.path(), r#"{"x":1}"#);
        install(d2.path(), &exe(), Some("0.170.0")).unwrap();
        uninstall(d2.path(), &exe()).unwrap();
        assert_eq!(read(d2.path()), json!({"x": 1}));
    }

    #[test]
    fn uninstall_missing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!uninstall(dir.path(), &exe()).unwrap().changed);
        assert!(!dir.path().join(HOOKS_FILE).exists());
    }

    #[test]
    fn garbage_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = "{ \"hooks\": { oops ";
        write(dir.path(), garbage);
        assert!(install(dir.path(), &exe(), Some("0.170.0")).is_err());
        assert!(uninstall(dir.path(), &exe()).is_err());
        assert_eq!(fs::read_to_string(dir.path().join(HOOKS_FILE)).unwrap(), garbage);
        assert!(!dir.path().join(BACKUP_FILE).exists() && !dir.path().join(TMP_FILE).exists());
        assert!(!is_installed(dir.path(), &exe()));
        for content in [r#"[1]"#, r#"{"hooks":[]}"#, r#"{"hooks":{"Stop":{}}}"#] {
            write(dir.path(), content);
            assert!(install(dir.path(), &exe(), Some("0.170.0")).is_err(), "{content}");
            assert_eq!(fs::read_to_string(dir.path().join(HOOKS_FILE)).unwrap(), content);
        }
    }

    #[test]
    fn backup_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER);
        let r1 = install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        assert!(r1.backup_created);
        let backup = dir.path().join(BACKUP_FILE);
        assert_eq!(fs::read_to_string(&backup).unwrap(), USER);
        let r2 = install(dir.path(), &PathBuf::from("D:/other/sich.exe"), Some("0.170.0")).unwrap();
        assert!(r2.changed && !r2.backup_created);
        assert!(!uninstall(dir.path(), &exe()).unwrap().backup_created);
        assert_eq!(fs::read_to_string(&backup).unwrap(), USER);
    }

    #[test]
    fn utf8_bom_and_empty_file_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "\u{feff}{\"x\":1}");
        install(dir.path(), &exe(), Some("0.170.0")).unwrap();
        assert_eq!(read(dir.path())["x"], 1);
        let d2 = tempfile::tempdir().unwrap();
        write(d2.path(), "  \n");
        install(d2.path(), &exe(), Some("0.170.0")).unwrap();
        assert!(is_installed(d2.path(), &exe()));
    }
}
