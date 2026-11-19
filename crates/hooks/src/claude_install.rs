//! Registers `sich hook claude` in Claude Code's `settings.json` (documented hooks mechanism).
//!
//! Safety rules: only our own entries are ever added, changed or removed; everything else
//! (user hooks, other tools' hooks, key order) is kept; a hook event key is written only if the
//! installed Claude version knows it (an unknown key makes old versions reject the whole file);
//! an unparsable `settings.json` is never touched; the first modification makes a backup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value, json};

const SETTINGS_FILE: &str = "settings.json";
const BACKUP_FILE: &str = "settings.json.sich-backup";
const TMP_FILE: &str = "settings.json.sich-tmp";
const HOOK_TIMEOUT_SECS: u64 = 5;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

type Version = (u32, u32, u32);

/// A Claude hook event we register.
#[derive(Debug, Clone, Copy)]
pub struct EventDef {
    pub name: &'static str,
    /// First Claude Code version that knows this event.
    pub min_version: Version,
    /// Tool-style events use the matcher `"*"`.
    pub matcher: bool,
    /// Part of the long-standing set installed when the version cannot be determined.
    pub safe: bool,
}

const fn ev(name: &'static str, min_version: Version, matcher: bool, safe: bool) -> EventDef {
    EventDef { name, min_version, matcher, safe }
}

/// Minimum versions follow Orca's table (measured against Claude's packed enums); `Notification`
/// is as old as hooks themselves.
pub const EVENTS: &[EventDef] = &[
    ev("PreToolUse", (1, 0, 23), true, true),
    ev("PostToolUse", (1, 0, 23), true, true),
    ev("Notification", (1, 0, 23), false, true),
    ev("Stop", (1, 0, 31), false, true),
    ev("SubagentStop", (1, 0, 41), false, true),
    ev("UserPromptSubmit", (1, 0, 53), false, true),
    ev("SessionStart", (1, 0, 62), false, true),
    ev("SessionEnd", (1, 0, 85), false, true),
    ev("SubagentStart", (2, 0, 43), false, false),
    ev("PermissionRequest", (2, 0, 45), true, false),
    ev("PostToolUseFailure", (2, 0, 56), true, false),
    ev("Elicitation", (2, 1, 63), true, false),
    ev("StopFailure", (2, 1, 78), false, false),
    ev("PostModelSwitch", (2, 1, 251), false, false),
];

/// Events that must carry our exact command for `is_installed`.
const CORE_EVENTS: &[&str] =
    &["SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "Notification", "Stop"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallReport {
    pub settings_path: PathBuf,
    pub claude_version: Option<String>,
    /// Events that now have our entry.
    pub installed: Vec<String>,
    /// Events whose entry of ours was removed (uninstall, or events the version does not know).
    pub removed: Vec<String>,
    /// False when `settings.json` already was in the desired state (nothing was written).
    pub changed: bool,
    pub backup_created: bool,
}

/// `CLAUDE_CONFIG_DIR` if set, else `~/.claude`.
pub fn default_claude_config_dir() -> Option<PathBuf> {
    config_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), dirs::home_dir())
}

fn config_dir_from(env: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    match env.filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => home.map(|h| h.join(".claude")),
    }
}

/// The command written into `settings.json`: `"<exe>" hook claude`.
pub fn hook_command(exe: &Path) -> String {
    format!("\"{}\" hook claude", exe.display())
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

/// Extracts `x.y.z` from `claude --version` output (`2.1.139 (Claude Code)`). A launcher that
/// prints its own version does not match.
pub fn parse_claude_version_output(output: &str) -> Option<String> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(idx) = lower.find("(claude code)") else { continue };
        let token = line[..idx].split_whitespace().last()?;
        if parse_version_triple(token).is_some() {
            return Some(token.to_string());
        }
    }
    None
}

/// The events to install for `version` (`None` or unparsable: only the long-standing safe set).
pub fn events_for_version(version: Option<&str>) -> Vec<&'static EventDef> {
    match version.and_then(parse_version_triple) {
        Some(v) => EVENTS.iter().filter(|e| e.min_version <= v).collect(),
        None => EVENTS.iter().filter(|e| e.safe).collect(),
    }
}

/// Runs `claude --version` (hidden window, 3 s timeout).
pub fn probe_claude_version() -> Option<String> {
    let mut cmd;
    if cfg!(windows) {
        // `claude` may be a `.cmd` shim (npm install), which only the shell resolves.
        cmd = Command::new("cmd");
        cmd.args(["/C", "claude", "--version"]);
    } else {
        cmd = Command::new("claude");
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
                return parse_claude_version_output(&text);
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

/// Splits a command line into words; double and single quotes group, no escapes (backslashes in
/// Windows paths stay literal).
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

/// True for commands whose program file stem is `sich` and whose args are `hook claude`.
pub fn is_our_command(cmd: &str) -> bool {
    let t = tokenize(cmd);
    t.len() == 3 && program_stem(&t[0]) == "sich" && t[1] == "hook" && t[2] == "claude"
}

fn is_our_hook(h: &Value) -> bool {
    h.get("command").and_then(Value::as_str).is_some_and(is_our_command)
}

// ---------------------------------------------------------------------------------------------
// settings.json merging

fn desired_hook(command: &str) -> Value {
    json!({ "type": "command", "command": command, "timeout": HOOK_TIMEOUT_SECS })
}

fn desired_group(def: &EventDef, hook: Value) -> Value {
    let mut g = Map::new();
    if def.matcher {
        g.insert("matcher".into(), json!("*"));
    }
    g.insert("hooks".into(), Value::Array(vec![hook]));
    Value::Object(g)
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
        bail!("settings.json is not a JSON object");
    };
    if !obj.contains_key("hooks") {
        if !create {
            return Ok(None);
        }
        obj.insert("hooks".into(), Value::Object(Map::new()));
    }
    match obj.get_mut("hooks") {
        Some(Value::Object(h)) => Ok(Some(h)),
        _ => bail!("`hooks` in settings.json is not an object"),
    }
}

/// Removes our entries from the events named in `events` (all hook events when `None`),
/// dropping arrays that we emptied. Returns the events that lost an entry.
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
                bail!("hooks.{name} in settings.json is not an array");
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
    let hook = desired_hook(command);
    let mut installed = Vec::new();
    for def in plan {
        let slot = hooks.entry(def.name).or_insert_with(|| Value::Array(Vec::new()));
        let Some(groups) = slot.as_array_mut() else {
            bail!("hooks.{} in settings.json is not an array", def.name);
        };
        let (placed, _) = strip_ours(groups, Some(&hook));
        if !placed {
            groups.push(desired_group(def, hook.clone()));
        }
        installed.push(def.name.to_string());
    }
    Ok((installed, removed))
}

// ---------------------------------------------------------------------------------------------
// File handling

fn settings_path(dir: &Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

/// Reads settings.json: `Ok(None)` when missing, an empty file counts as `{}`.
fn read_settings(path: &Path) -> anyhow::Result<Option<Value>> {
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
fn write_settings(dir: &Path, root: &Value, existed: bool) -> anyhow::Result<bool> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = settings_path(dir);
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

/// Installs for the detected Claude version (`claude --version`).
pub fn install(claude_config_dir: &Path, exe: &Path) -> anyhow::Result<InstallReport> {
    install_with_version(claude_config_dir, exe, probe_claude_version().as_deref())
}

/// Same as [`install`] with the version given (`None` = unknown).
pub fn install_with_version(
    claude_config_dir: &Path,
    exe: &Path,
    version: Option<&str>,
) -> anyhow::Result<InstallReport> {
    let path = settings_path(claude_config_dir);
    let original = read_settings(&path)?;
    let existed = original.is_some();
    let mut root = original.clone().unwrap_or_else(|| Value::Object(Map::new()));

    let plan = events_for_version(version);
    // Only a known version may retire events; an unknown one must not undo earlier work.
    let retire: Vec<&str> = if version.and_then(parse_version_triple).is_some() {
        EVENTS.iter().filter(|e| !plan.iter().any(|p| p.name == e.name)).map(|e| e.name).collect()
    } else {
        Vec::new()
    };
    let (installed, removed) = merge_install(&mut root, &hook_command(exe), &plan, &retire)?;

    let mut report = InstallReport {
        settings_path: path,
        claude_version: version.map(str::to_string),
        installed,
        removed,
        ..Default::default()
    };
    if original.as_ref() != Some(&root) {
        report.backup_created = write_settings(claude_config_dir, &root, existed)?;
        report.changed = true;
    }
    Ok(report)
}

/// Removes our entries only. `exe` is accepted for symmetry: entries are recognised by shape,
/// so stale paths are removed as well.
pub fn uninstall(claude_config_dir: &Path, _exe: &Path) -> anyhow::Result<InstallReport> {
    let path = settings_path(claude_config_dir);
    let mut report = InstallReport { settings_path: path.clone(), ..Default::default() };
    let Some(original) = read_settings(&path)? else { return Ok(report) };
    let mut root = original.clone();
    if let Some(hooks) = hooks_object(&mut root, false)? {
        report.removed = remove_ours(hooks, None)?;
        if hooks.is_empty() && !report.removed.is_empty() {
            root.as_object_mut().expect("object").shift_remove("hooks");
        }
    }
    if root != original {
        report.backup_created = write_settings(claude_config_dir, &root, true)?;
        report.changed = true;
    }
    Ok(report)
}

/// True if the core events carry exactly the hook command for `exe` (a stale path counts as
/// not installed, so the UI offers a reinstall).
pub fn is_installed(claude_config_dir: &Path, exe: &Path) -> bool {
    let Ok(Some(root)) = read_settings(&settings_path(claude_config_dir)) else { return false };
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
    const NEW_VERSION: &str = "2.1.300";

    fn exe() -> PathBuf {
        PathBuf::from(EXE)
    }

    fn read(dir: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap()).unwrap()
    }

    fn write(dir: &Path, text: &str) {
        fs::write(dir.join(SETTINGS_FILE), text).unwrap();
    }

    fn all_commands(v: &Value) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(hooks) = v.get("hooks").and_then(Value::as_object) {
            for groups in hooks.values() {
                for g in groups.as_array().unwrap() {
                    for h in g["hooks"].as_array().unwrap() {
                        out.push(h["command"].as_str().unwrap().to_string());
                    }
                }
            }
        }
        out
    }

    fn ours_count(v: &Value) -> usize {
        all_commands(v).iter().filter(|c| is_our_command(c)).count()
    }

    #[test]
    fn command_format_quotes_exe() {
        assert_eq!(hook_command(&exe()), r#""C:\Apps\Sich\sich.exe" hook claude"#);
    }

    #[test]
    fn recognises_our_commands() {
        assert!(is_our_command(r#""C:\a b\sich.exe" hook claude"#));
        assert!(is_our_command(r#""C:\a\target\debug\sich.exe" hook claude"#));
        assert!(is_our_command("/usr/local/bin/sich hook claude"));
        assert!(is_our_command("sich hook claude"));
        assert!(is_our_command(r#""D:\SICH.EXE" hook claude"#));
        assert!(!is_our_command(r#""C:\a\sich.exe" hook gemini"#));
        assert!(!is_our_command(r#""C:\a\sich.exe" daemon"#));
        assert!(!is_our_command(r#""C:\a\orca-hook.exe" hook claude"#));
        assert!(!is_our_command("node ~/.orca/hook.js claude"));
        assert!(!is_our_command(r#""C:\a\sich.exe" hook claude --extra"#));
        assert!(!is_our_command(""));
    }

    #[test]
    fn config_dir_resolution() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(config_dir_from(Some("/x/cfg".into()), home.clone()), Some(PathBuf::from("/x/cfg")));
        assert_eq!(config_dir_from(None, home.clone()), Some(PathBuf::from("/home/u/.claude")));
        assert_eq!(config_dir_from(Some("".into()), home), Some(PathBuf::from("/home/u/.claude")));
        assert_eq!(config_dir_from(None, None), None);
    }

    #[test]
    fn version_output_parsing() {
        assert_eq!(parse_claude_version_output("2.1.139 (Claude Code)\n"), Some("2.1.139".into()));
        assert_eq!(parse_claude_version_output("warn\n1.0.62 (claude code)"), Some("1.0.62".into()));
        assert_eq!(parse_claude_version_output("3.0.0-beta.1 (Claude Code)"), Some("3.0.0-beta.1".into()));
        assert_eq!(parse_claude_version_output("npx 10.2.0"), None);
        assert_eq!(parse_claude_version_output(""), None);
        assert_eq!(parse_claude_version_output("abc (Claude Code)"), None);
    }

    fn names(v: &[&EventDef]) -> Vec<&'static str> {
        v.iter().map(|e| e.name).collect()
    }

    #[test]
    fn version_gating() {
        let safe = [
            "PreToolUse",
            "PostToolUse",
            "Notification",
            "Stop",
            "SubagentStop",
            "UserPromptSubmit",
            "SessionStart",
            "SessionEnd",
        ];
        // Unknown or garbage version: only the safe set.
        for v in [None, Some("garbage"), Some("")] {
            let mut got = names(&events_for_version(v));
            got.sort_unstable();
            let mut want = safe.to_vec();
            want.sort_unstable();
            assert_eq!(got, want, "{v:?}");
        }
        let has = |v: &str, n: &str| names(&events_for_version(Some(v))).contains(&n);
        assert!(!has("1.0.22", "PreToolUse"));
        assert!(has("1.0.23", "PreToolUse"));
        assert!(!has("1.0.52", "UserPromptSubmit"));
        assert!(has("1.0.53", "UserPromptSubmit"));
        assert!(!has("1.0.84", "SessionEnd"));
        assert!(has("1.0.85", "SessionEnd"));
        assert!(!has("2.0.42", "SubagentStart"));
        assert!(has("2.0.43", "SubagentStart"));
        assert!(!has("2.0.44", "PermissionRequest"));
        assert!(has("2.0.45", "PermissionRequest"));
        assert!(!has("2.0.55", "PostToolUseFailure"));
        assert!(has("2.0.56", "PostToolUseFailure"));
        assert!(!has("2.1.62", "Elicitation"));
        assert!(has("2.1.63", "Elicitation"));
        assert!(!has("2.1.77", "StopFailure"));
        assert!(has("2.1.78", "StopFailure"));
        assert!(has("2.1.139-beta.2", "StopFailure"));
        assert!(!has("2.1.250", "PostModelSwitch"));
        assert!(has("2.1.251", "PostModelSwitch"));
        assert!(has("2.2.0", "PostModelSwitch"));
        assert!(!has("2.1.139", "PostModelSwitch"));
        assert!(!names(&events_for_version(None)).contains(&"PostModelSwitch"));
        // Newest: everything.
        assert_eq!(events_for_version(Some(NEW_VERSION)).len(), EVENTS.len());
        // Every event name is unique and only known names are ever installed.
        let mut all = names(&EVENTS.iter().collect::<Vec<_>>());
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), EVENTS.len());
    }

    #[test]
    fn install_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("nested").join(".claude");
        let r = install_with_version(&cfg, &exe(), Some(NEW_VERSION)).unwrap();
        assert!(r.changed);
        assert!(!r.backup_created, "nothing to back up");
        assert!(!cfg.join(BACKUP_FILE).exists());
        let v = read(&cfg);
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), EVENTS.len());
        let entry = &hooks["Stop"][0]["hooks"][0];
        assert_eq!(entry["type"], "command");
        assert_eq!(entry["command"], hook_command(&exe()));
        assert_eq!(entry["timeout"], 5);
        assert!(hooks["Stop"][0].get("matcher").is_none());
        assert_eq!(hooks["PreToolUse"][0]["matcher"], "*");
        assert_eq!(hooks["PostToolUse"][0]["matcher"], "*");
        assert_eq!(hooks["PermissionRequest"][0]["matcher"], "*");
        assert!(is_installed(&cfg, &exe()));
        assert!(!cfg.join(TMP_FILE).exists());
    }

    #[test]
    fn install_into_empty_object_and_empty_file() {
        for content in ["{}", "", "  \n"] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), content);
            let r = install_with_version(dir.path(), &exe(), None).unwrap();
            assert!(r.changed);
            assert_eq!(read(dir.path())["hooks"].as_object().unwrap().len(), 8);
        }
    }

    #[test]
    fn unknown_version_never_writes_new_events() {
        let dir = tempfile::tempdir().unwrap();
        install_with_version(dir.path(), &exe(), None).unwrap();
        let v = read(dir.path());
        let keys: Vec<&String> = v["hooks"].as_object().unwrap().keys().collect();
        for new in ["SubagentStart", "PermissionRequest", "PostToolUseFailure", "Elicitation", "StopFailure"] {
            assert!(!keys.iter().any(|k| *k == new), "{new} must not be written");
        }
        assert!(keys.iter().any(|k| *k == "Notification"));
    }

    const USER_SETTINGS: &str = r#"{
  "zeta": 1,
  "model": "opus",
  "hooks": {
    "Stop": [
      { "hooks": [ { "type": "command", "command": "echo user-stop" } ] }
    ],
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [ { "type": "command", "command": "node ~/.orca/hook.js", "timeout": 10 } ] }
    ],
    "CustomUserEvent": [ { "hooks": [] } ]
  },
  "alpha": { "x": [1, 2, 3] }
}"#;

    #[test]
    fn user_hooks_and_key_order_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER_SETTINGS);
        let before: Value = serde_json::from_str(USER_SETTINGS).unwrap();
        install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        let after = read(dir.path());

        // Top-level key order untouched.
        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "model", "hooks", "alpha"]);
        assert_eq!(after["alpha"], before["alpha"]);
        assert_eq!(after["zeta"], before["zeta"]);
        // Existing event keys keep their position at the front, new ones are appended.
        let hook_keys: Vec<&String> = after["hooks"].as_object().unwrap().keys().collect();
        assert_eq!(&hook_keys[..3], ["Stop", "PreToolUse", "CustomUserEvent"]);
        // User groups are intact and first in their arrays.
        assert_eq!(after["hooks"]["Stop"][0], before["hooks"]["Stop"][0]);
        assert_eq!(after["hooks"]["PreToolUse"][0], before["hooks"]["PreToolUse"][0]);
        assert_eq!(after["hooks"]["CustomUserEvent"], before["hooks"]["CustomUserEvent"]);
        assert_eq!(after["hooks"]["Stop"].as_array().unwrap().len(), 2);
        let cmds = all_commands(&after);
        assert!(cmds.contains(&"echo user-stop".to_string()));
        assert!(cmds.contains(&"node ~/.orca/hook.js".to_string()));
        assert_eq!(ours_count(&after), EVENTS.len());
    }

    #[test]
    fn reinstall_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER_SETTINGS);
        install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        let first = fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        let r = install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        assert!(!r.changed);
        assert!(!r.backup_created);
        let second = fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap();
        assert_eq!(first, second);
        assert_eq!(ours_count(&read(dir.path())), EVENTS.len());
    }

    #[test]
    fn changed_path_replaces_the_old_entry() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER_SETTINGS);
        let old = PathBuf::from(r"C:\dev\sich\target\debug\sich.exe");
        install_with_version(dir.path(), &old, Some(NEW_VERSION)).unwrap();
        assert!(is_installed(dir.path(), &old));
        let r = install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        assert!(r.changed);
        let v = read(dir.path());
        assert_eq!(ours_count(&v), EVENTS.len(), "no duplicates");
        let cmds = all_commands(&v);
        assert!(!cmds.iter().any(|c| c.contains("debug")));
        assert!(is_installed(dir.path(), &exe()));
        assert!(!is_installed(dir.path(), &old));
        // Replaced in place: still the second group of Stop.
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn duplicate_entries_collapse_to_one() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            r#"{"hooks":{"Stop":[
                {"hooks":[{"type":"command","command":"\"C:\\a\\sich.exe\" hook claude"}]},
                {"hooks":[{"type":"command","command":"echo keep"},{"type":"command","command":"\"C:\\b\\sich.exe\" hook claude"}]}
            ]}}"#,
        );
        install_with_version(dir.path(), &exe(), None).unwrap();
        let v = read(dir.path());
        let stop = v["hooks"]["Stop"].as_array().unwrap();
        let stop_cmds: Vec<&str> = stop
            .iter()
            .flat_map(|g| g["hooks"].as_array().unwrap())
            .map(|h| h["command"].as_str().unwrap())
            .collect();
        assert_eq!(stop_cmds.iter().filter(|c| is_our_command(c)).count(), 1);
        assert!(stop_cmds.contains(&"echo keep"));
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER_SETTINGS);
        let before: Value = serde_json::from_str(USER_SETTINGS).unwrap();
        install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        let r = uninstall(dir.path(), &exe()).unwrap();
        assert!(r.changed);
        assert_eq!(read(dir.path()), before, "back to the original meaning");
        assert!(!is_installed(dir.path(), &exe()));
        // Second uninstall changes nothing.
        assert!(!uninstall(dir.path(), &exe()).unwrap().changed);
    }

    #[test]
    fn uninstall_drops_arrays_and_hooks_object_we_created() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"model":"opus"}"#);
        install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        uninstall(dir.path(), &exe()).unwrap();
        assert_eq!(read(dir.path()), json!({"model": "opus"}));
    }

    #[test]
    fn uninstall_keeps_non_empty_user_arrays() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), r#"{"hooks":{"Notification":[{"hooks":[{"type":"command","command":"echo n"}]}]}}"#);
        let before = read(dir.path());
        install_with_version(dir.path(), &exe(), None).unwrap();
        uninstall(dir.path(), &exe()).unwrap();
        assert_eq!(read(dir.path()), before);
    }

    #[test]
    fn uninstall_missing_file_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let r = uninstall(dir.path(), &exe()).unwrap();
        assert!(!r.changed);
        assert!(!dir.path().join(SETTINGS_FILE).exists());
    }

    #[test]
    fn invalid_json_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = "{ \"hooks\": { oops ";
        write(dir.path(), garbage);
        assert!(install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).is_err());
        assert!(uninstall(dir.path(), &exe()).is_err());
        assert_eq!(fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(), garbage);
        assert!(!dir.path().join(BACKUP_FILE).exists());
        assert!(!dir.path().join(TMP_FILE).exists());
        assert!(!is_installed(dir.path(), &exe()));
    }

    #[test]
    fn wrong_shapes_are_errors_not_overwrites() {
        for content in [r#"[1,2]"#, r#"{"hooks":[]}"#, r#"{"hooks":{"Stop":{}}}"#, r#""str""#] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), content);
            assert!(install_with_version(dir.path(), &exe(), None).is_err(), "{content}");
            assert_eq!(fs::read_to_string(dir.path().join(SETTINGS_FILE)).unwrap(), content);
        }
    }

    #[test]
    fn backup_is_created_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER_SETTINGS);
        let r1 = install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        assert!(r1.backup_created);
        let backup = dir.path().join(BACKUP_FILE);
        assert_eq!(fs::read_to_string(&backup).unwrap(), USER_SETTINGS);
        // A later change (new path) must not overwrite the backup.
        let r2 = install_with_version(dir.path(), &PathBuf::from("D:/other/sich.exe"), Some(NEW_VERSION)).unwrap();
        assert!(r2.changed);
        assert!(!r2.backup_created);
        assert_eq!(fs::read_to_string(&backup).unwrap(), USER_SETTINGS);
        let r3 = uninstall(dir.path(), &exe()).unwrap();
        assert!(!r3.backup_created);
        assert_eq!(fs::read_to_string(&backup).unwrap(), USER_SETTINGS);
    }

    #[test]
    fn downgrade_retires_unsupported_events_only_for_known_versions() {
        let dir = tempfile::tempdir().unwrap();
        install_with_version(dir.path(), &exe(), Some(NEW_VERSION)).unwrap();
        // Unknown version: leaves everything.
        let r = install_with_version(dir.path(), &exe(), None).unwrap();
        assert!(r.removed.is_empty());
        assert_eq!(ours_count(&read(dir.path())), EVENTS.len());
        // Known old version: unsupported events are removed.
        let r = install_with_version(dir.path(), &exe(), Some("2.0.50")).unwrap();
        assert!(r.removed.contains(&"StopFailure".to_string()));
        let v = read(dir.path());
        assert!(v["hooks"].get("StopFailure").is_none());
        assert!(v["hooks"].get("PermissionRequest").is_some());
        assert_eq!(ours_count(&v), events_for_version(Some("2.0.50")).len());
    }

    #[test]
    fn is_installed_false_when_missing_or_partial() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_installed(dir.path(), &exe()));
        write(dir.path(), "{}");
        assert!(!is_installed(dir.path(), &exe()));
        install_with_version(dir.path(), &exe(), None).unwrap();
        assert!(is_installed(dir.path(), &exe()));
    }

    #[test]
    fn reinstall_adds_post_model_switch_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), USER_SETTINGS);
        // Older Claude: no PostModelSwitch.
        install_with_version(dir.path(), &exe(), Some("2.1.200")).unwrap();
        assert!(read(dir.path())["hooks"].get("PostModelSwitch").is_none());
        // Upgraded Claude: the event is added, nothing else duplicated.
        let r = install_with_version(dir.path(), &exe(), Some("2.1.251")).unwrap();
        assert!(r.changed);
        let v = read(dir.path());
        assert_eq!(v["hooks"]["PostModelSwitch"].as_array().unwrap().len(), 1);
        assert!(v["hooks"]["PostModelSwitch"][0].get("matcher").is_none());
        assert_eq!(ours_count(&v), events_for_version(Some("2.1.251")).len());
        let r = install_with_version(dir.path(), &exe(), Some("2.1.251")).unwrap();
        assert!(!r.changed);
        assert_eq!(read(dir.path())["hooks"]["PostModelSwitch"].as_array().unwrap().len(), 1);
        // Uninstall removes it too.
        uninstall(dir.path(), &exe()).unwrap();
        assert!(read(dir.path())["hooks"].get("PostModelSwitch").is_none());
    }

    #[test]
    fn utf8_bom_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "\u{feff}{\"model\":\"x\"}");
        install_with_version(dir.path(), &exe(), None).unwrap();
        assert_eq!(read(dir.path())["model"], "x");
    }
}
