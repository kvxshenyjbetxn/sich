//! Ownership and restoration of the Claude statusLine slot.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    SETTINGS_EDIT, Value, json, parse_version_triple, program_stem, read_settings, settings_path,
    write_settings,
};
use crate::statusline::{decode_command, encode_command, write_json};

const ORIGINAL: &str = "claude-statusline-original.json";
const OWNER: &str = "claude-statusline-owner.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatuslineState {
    Off,
    Unavailable,
    Installed,
    NeedsWrap,
    UserEdited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Owner {
    Sich,
    SichWithWrap,
    UserEdited,
    WrapDisabled,
}

fn user_command(value: &Value) -> Option<&str> {
    if value["type"].as_str() != Some("command") {
        return None;
    }
    value["command"].as_str()
}

const POWERSHELL_PREFIX: &str = "$p=[Diagnostics.Process]::Start([Diagnostics.ProcessStartInfo]@{FileName='";
const POWERSHELL_ARGUMENTS: &str = "';Arguments='statusline";
const POWERSHELL_SUFFIX: &str = "';UseShellExecute=$false});$p.WaitForExit();exit $p.ExitCode";

/// Git Bash and POSIX use the same quoted executable as Sich hooks. The Windows PowerShell
/// fallback must wait for the GUI-subsystem executable explicitly, with its stdio inherited.
fn command(exe: &Path, original: Option<&str>) -> String {
    let path = exe.to_string_lossy();
    #[cfg(windows)]
    let path = path.replace('\\', "/");
    #[cfg(windows)]
    let powershell = crate::statusline::git_bash().is_none();
    #[cfg(not(windows))]
    let powershell = false;

    let original = original.map(|command| format!(" -- {}", encode_command(command))).unwrap_or_default();
    if powershell {
        let path = path.replace('\'', "''");
        return format!("{POWERSHELL_PREFIX}{path}{POWERSHELL_ARGUMENTS}{original}{POWERSHELL_SUFFIX}");
    }
    let mut invocation = String::from("\"");
    for c in path.chars() {
        if matches!(c, '"' | '$' | '`' | '\\') {
            invocation.push('\\');
        }
        invocation.push(c);
    }
    invocation.push_str("\" statusline");
    invocation.push_str(&original);
    invocation
}

enum Managed {
    Standalone,
    Wrapped(String),
}

fn literal_program(input: &str) -> Option<String> {
    if let Some(quoted) = input.strip_prefix('"') {
        let mut chars = quoted.strip_suffix('"')?.chars().peekable();
        let mut decoded = String::new();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek().is_some_and(|c| matches!(c, '"' | '$' | '`' | '\\')) {
                decoded.push(chars.next()?);
            } else if matches!(c, '"' | '$' | '`' | '\r' | '\n') {
                return None;
            } else {
                decoded.push(c);
            }
        }
        return Some(decoded);
    }
    if let Some(quoted) = input.strip_prefix('\'') {
        let path = quoted.strip_suffix('\'')?;
        return (!path.contains('\'')).then(|| path.to_owned());
    }
    if input.chars().any(|c| c.is_whitespace() || "'\"$`|;&<>()".contains(c)) {
        return None;
    }
    Some(input.to_owned())
}

fn installed_command(command: &str) -> Option<(String, &str)> {
    if let Some(powershell) = command.strip_prefix(POWERSHELL_PREFIX) {
        let (program, args) =
            powershell.strip_suffix(POWERSHELL_SUFFIX)?.rsplit_once(POWERSHELL_ARGUMENTS)?;
        let mut chars = program.chars();
        let mut path = String::new();
        while let Some(c) = chars.next() {
            if c == '\'' && chars.next() != Some('\'') {
                return None;
            }
            path.push(c);
        }
        return Some((path, args));
    }
    let (program, args) = command.rsplit_once(" statusline")?;
    Some((literal_program(program)?, args))
}

fn managed(value: &Value, exe: &Path) -> Option<Managed> {
    let (program, args) = installed_command(user_command(value)?.trim())?;
    if program_stem(&program) != "sich" && Path::new(&program) != exe {
        return None;
    }
    if args.is_empty() {
        Some(Managed::Standalone)
    } else {
        decode_command(args.strip_prefix(" -- ")?).map(Managed::Wrapped)
    }
}

fn remove_if_present(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => Ok(result?),
    }
}

/// Reconciles user-level Claude settings. The original object is saved before publishing a
/// wrapper and removed only after the settings transaction no longer depends on it.
pub fn update_statusline(
    claude_dir: &Path,
    sich_dir: &Path,
    exe: &Path,
    version: Option<&str>,
    enabled: bool,
    wrap: bool,
) -> anyhow::Result<StatuslineState> {
    if !version.and_then(parse_version_triple).is_some_and(|version| version >= (2, 1, 80)) {
        return Ok(StatuslineState::Unavailable);
    }
    let _edit = SETTINGS_EDIT.lock().map_err(|_| anyhow::anyhow!("Claude settings edit lock poisoned"))?;
    let previous = read_settings(&settings_path(claude_dir))?;
    let mut settings = previous.clone().unwrap_or_else(|| json!({}));
    let slots =
        settings.as_object_mut().ok_or_else(|| anyhow::anyhow!("settings.json is not a JSON object"))?;
    let original_path = sich_dir.join(ORIGINAL);
    let owner_path = sich_dir.join(OWNER);
    let owner = read_settings(&owner_path)?.and_then(|value| serde_json::from_value(value).ok());
    let current = slots.get("statusLine").cloned().map(|entry| {
        let managed = managed(&entry, exe);
        (entry, managed)
    });

    let sich = Some(if wrap { Owner::SichWithWrap } else { Owner::Sich });
    let (state, next_owner, keep_original) = match current {
        Some((mut entry, Some(Managed::Wrapped(original)))) if enabled && wrap => {
            entry["command"] = json!(command(exe, Some(&original)));
            slots.insert("statusLine".into(), entry);
            (StatuslineState::Installed, sich, true)
        }
        Some((mut entry, Some(Managed::Wrapped(original)))) => {
            entry["command"] = json!(original);
            let original = read_settings(&original_path)?.unwrap_or(entry);
            slots.insert("statusLine".into(), original);
            let state = if enabled { StatuslineState::NeedsWrap } else { StatuslineState::Off };
            (state, None, false)
        }
        Some((mut entry, Some(Managed::Standalone)))
            if enabled && (wrap || owner != Some(Owner::SichWithWrap)) =>
        {
            entry["command"] = json!(command(exe, None));
            slots.insert("statusLine".into(), entry);
            (StatuslineState::Installed, sich, false)
        }
        Some((_, Some(Managed::Standalone))) => {
            slots.shift_remove("statusLine");
            (StatuslineState::Off, enabled.then_some(Owner::WrapDisabled), false)
        }
        _ if !enabled => (StatuslineState::Off, None, false),
        // An edit to a previously managed slot wins over the current opt-in. Clearing the
        // wrap setting lets the user explicitly opt in again.
        None if owner == Some(Owner::WrapDisabled) && !wrap => (StatuslineState::Off, owner, false),
        current if matches!(owner, Some(Owner::Sich | Owner::SichWithWrap | Owner::UserEdited)) => {
            if current.is_some() && !wrap {
                (StatuslineState::NeedsWrap, None, false)
            } else {
                (StatuslineState::UserEdited, Some(Owner::UserEdited), false)
            }
        }
        None => {
            slots.insert("statusLine".into(), json!({"type": "command", "command": command(exe, None)}));
            (StatuslineState::Installed, sich, false)
        }
        Some((mut entry, None)) if wrap && user_command(&entry).is_some() => {
            let original = user_command(&entry).expect("validated command");
            let wrapped = command(exe, Some(original));
            write_json(&original_path, &entry)?;
            entry["command"] = json!(wrapped);
            slots.insert("statusLine".into(), entry);
            (StatuslineState::Installed, sich, true)
        }
        Some((_, None)) => (StatuslineState::NeedsWrap, None, false),
    };

    if settings != previous.clone().unwrap_or_else(|| json!({})) {
        write_settings(claude_dir, &settings, previous.is_some())?;
    }
    if !keep_original {
        remove_if_present(&original_path)?;
    }
    if next_owner != owner {
        match next_owner {
            Some(owner) => write_json(&owner_path, &owner)?,
            None => remove_if_present(&owner_path)?,
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSION: Option<&str> = Some("2.1.80");

    struct Fixture {
        claude: tempfile::TempDir,
        sich: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self { claude: tempfile::tempdir().unwrap(), sich: tempfile::tempdir().unwrap() }
        }

        fn exe(&self) -> std::path::PathBuf {
            self.sich.path().join("old and spaced").join("sich.exe")
        }

        fn settings(&self, value: &Value) {
            fs::write(settings_path(self.claude.path()), value.to_string()).unwrap();
        }

        fn slot(&self) -> Option<Value> {
            read_settings(&settings_path(self.claude.path())).unwrap()?.get("statusLine").cloned()
        }

        fn apply(&self, enabled: bool, wrap: bool) -> StatuslineState {
            update_statusline(self.claude.path(), self.sich.path(), &self.exe(), VERSION, enabled, wrap)
                .unwrap()
        }
    }

    #[test]
    fn missing_slot_gets_owned_entry_and_disabling_removes_it() {
        let f = Fixture::new();
        f.settings(&json!({"somethingElse": true}));
        assert_eq!(f.apply(true, false), StatuslineState::Installed);
        assert_eq!(f.slot(), Some(json!({"type":"command", "command": command(&f.exe(), None)})));
        assert_eq!(f.apply(false, false), StatuslineState::Off);
        assert_eq!(f.slot(), None);
        assert!(
            read_settings(&settings_path(f.claude.path())).unwrap().unwrap()["somethingElse"]
                .as_bool()
                .unwrap()
        );
    }

    #[test]
    fn turning_wrap_off_removes_the_standalone_entry_installed_while_wrap_was_on() {
        let f = Fixture::new();
        assert_eq!(f.apply(true, true), StatuslineState::Installed);
        assert_eq!(f.apply(true, false), StatuslineState::Off);
        assert_eq!(f.apply(true, false), StatuslineState::Off);
        assert_eq!(f.slot(), None);
        assert_eq!(f.apply(true, true), StatuslineState::Installed);
    }

    #[test]
    fn wrapping_preserves_fields_backs_up_once_and_restores_original() {
        let f = Fixture::new();
        let custom =
            json!({"type":"command","command":"printf '%s' \"$HOME\" | cat","padding":3,"extra":[1,false]});
        let settings = json!({"statusLine": custom});
        f.settings(&settings);
        assert_eq!(f.apply(true, false), StatuslineState::NeedsWrap);
        assert_eq!(f.slot(), Some(custom.clone()));
        let backup = f.claude.path().join(super::super::BACKUP_FILE);
        assert!(!backup.exists());
        assert_eq!(f.apply(true, true), StatuslineState::Installed);
        let wrapped = f.slot().unwrap();
        assert_eq!(wrapped["padding"], 3);
        assert_eq!(wrapped["extra"], custom["extra"]);
        assert_eq!(wrapped["command"], command(&f.exe(), user_command(&custom)));
        assert_eq!(read_settings(&f.sich.path().join(ORIGINAL)).unwrap(), Some(custom.clone()));
        assert_eq!(f.apply(true, false), StatuslineState::NeedsWrap);
        assert_eq!(f.slot(), Some(custom.clone()));
        assert!(!f.sich.path().join(ORIGINAL).exists());
        assert_eq!(f.apply(true, true), StatuslineState::Installed);
        assert_eq!(f.apply(false, true), StatuslineState::Off);
        assert_eq!(f.slot(), Some(custom));
        assert_eq!(read_settings(&backup).unwrap(), Some(settings));
    }

    #[test]
    fn leaves_user_edits_alone_and_drops_the_saved_original() {
        let f = Fixture::new();
        f.settings(&json!({"statusLine":{"type":"command","command":"first"}}));
        f.apply(true, true);
        let new = json!({"type":"command","command":"second","padding":2});
        f.settings(&json!({"statusLine":new}));
        assert_eq!(f.apply(true, true), StatuslineState::UserEdited);
        assert_eq!(f.apply(true, true), StatuslineState::UserEdited);
        assert!(!f.sich.path().join(ORIGINAL).exists());
        assert_eq!(f.slot(), Some(new.clone()));
        assert_eq!(f.apply(false, true), StatuslineState::Off);
        assert_eq!(f.slot(), Some(new));
    }

    #[test]
    fn updating_exe_path_rewrites_entry_without_changing_original() {
        let f = Fixture::new();
        let new_exe = f.sich.path().join("new/sich.exe");
        let original = json!({"type":"command","command":"my command","padding":5});
        f.settings(&json!({"statusLine":original}));
        f.apply(true, true);
        update_statusline(f.claude.path(), f.sich.path(), &new_exe, VERSION, true, true).unwrap();
        assert_eq!(f.slot().unwrap()["command"], command(&new_exe, Some("my command")));
        f.apply(false, false);
        assert_eq!(f.slot(), Some(original));
    }

    #[test]
    fn an_executable_name_inside_a_user_shell_program_does_not_establish_ownership() {
        let f = Fixture::new();
        for command in [
            "echo value | /tmp/sich statusline",
            "echo /tmp/sich statusline",
            r#""/tmp/foo" | "/tmp/sich" statusline"#,
        ] {
            let user = json!({"type":"command","command":command});
            f.settings(&json!({"statusLine":user}));
            assert_eq!(f.apply(false, false), StatuslineState::Off);
            assert_eq!(f.slot(), Some(user));
        }
    }

    #[test]
    fn too_old_and_unparsable_settings_stay_untouched() {
        let f = Fixture::new();
        let path = settings_path(f.claude.path());
        assert_eq!(
            update_statusline(f.claude.path(), f.sich.path(), &f.exe(), Some("2.1.79"), true, true).unwrap(),
            StatuslineState::Unavailable
        );
        assert!(!path.exists());
        fs::write(&path, "{broken").unwrap();
        assert!(update_statusline(f.claude.path(), f.sich.path(), &f.exe(), VERSION, true, true).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "{broken");
        assert_eq!(fs::read_dir(f.claude.path()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(f.sich.path()).unwrap().count(), 0);
    }
}
