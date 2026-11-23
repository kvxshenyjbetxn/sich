//! Installs the bundled export-only Claude Code mod (`sich-context`) through Claude's plugin
//! directory list: the files go to `<sich config>/claude-mod/sich-context/` and that folder is
//! added to `env.CLAUDE_CODE_PLUGIN_DIRS` in Claude's `settings.json`.
//!
//! Same safety rules as `claude_install`: only our own entry is ever added or removed, invalid
//! JSON is never touched, the first change makes the `settings.json.sich-backup` copy, writes are
//! atomic, key order is kept and edits are serialized with the hook installer.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::{Map, Value};

use crate::claude_install::{SETTINGS_EDIT, parse_version_triple, read_settings, settings_path, write_settings};

/// First Claude Code version the mod is installed for (its plugin engine API is newer than that).
pub const MOD_MIN_CLAUDE_VERSION: &str = "2.1.300";
const ENV_KEY: &str = "CLAUDE_CODE_PLUGIN_DIRS";
/// Normalized tail of our folder, used to recognize our entry wherever the Sich config lived.
const OUR_SUFFIX: &str = "/claude-mod/sich-context";

const FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", include_str!("../assets/claude-mod/sich-context/.claude-plugin/plugin.json")),
    ("hooks/hooks.json", include_str!("../assets/claude-mod/sich-context/hooks/hooks.json")),
    ("hooks/register.tsx", include_str!("../assets/claude-mod/sich-context/hooks/register.tsx")),
    ("types/index.d.ts", include_str!("../assets/claude-mod/sich-context/types/index.d.ts")),
    ("tsconfig.json", include_str!("../assets/claude-mod/sich-context/tsconfig.json")),
    ("README.md", include_str!("../assets/claude-mod/sich-context/README.md")),
    (".gitignore", include_str!("../assets/claude-mod/sich-context/.gitignore")),
];

/// Entry separator of the list: `;` on Windows (like `PATH`), `:` elsewhere.
const LIST_SEPARATOR: char = if cfg!(windows) { ';' } else { ':' };

fn mod_dir(sich_config_dir: &Path) -> PathBuf {
    sich_config_dir.join("claude-mod").join("sich-context")
}

/// The list entry for our folder: absolute, forward slashes.
fn entry_for(dir: &Path) -> String {
    dir.to_string_lossy().replace('\\', "/")
}

/// Comparison key: slash-normalized without a trailing slash; case-insensitive on Windows.
fn key(entry: &str, case_insensitive: bool) -> String {
    let normal = entry.trim().replace('\\', "/");
    let normal = normal.trim_end_matches('/');
    if case_insensitive { normal.to_lowercase() } else { normal.to_owned() }
}

/// Our folder, wherever the Sich config used to live.
fn is_ours(entry: &str, case_insensitive: bool) -> bool {
    key(entry, case_insensitive).ends_with(OUR_SUFFIX)
}

/// Adds (or refreshes) our entry; `None` when the list already is right. Other entries keep
/// their order and text; a stale entry of ours is replaced in place and duplicates are dropped.
fn add_entry(list: &str, entry: &str, sep: char, case_insensitive: bool) -> Option<String> {
    let want = key(entry, case_insensitive);
    let mut out: Vec<&str> = Vec::new();
    let mut placed = false;
    for item in list.split(sep).map(str::trim).filter(|item| !item.is_empty()) {
        if is_ours(item, case_insensitive) {
            if !placed {
                placed = true;
                out.push(if key(item, case_insensitive) == want { item } else { entry });
            }
        } else {
            out.push(item);
        }
    }
    if !placed {
        out.push(entry);
    }
    let next = out.join(&sep.to_string());
    (next != list).then_some(next)
}

/// Removes our entries; `None` when there were none. The result may be empty.
fn remove_entries(list: &str, sep: char, case_insensitive: bool) -> Option<String> {
    let items: Vec<&str> = list.split(sep).map(str::trim).filter(|item| !item.is_empty()).collect();
    let kept: Vec<&str> = items.iter().copied().filter(|item| !is_ours(item, case_insensitive)).collect();
    (kept.len() != items.len()).then(|| kept.join(&sep.to_string()))
}

fn write_files(dir: &Path) -> anyhow::Result<()> {
    for (rel, content) in FILES {
        let path = dir.join(rel);
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == *content) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

fn env_object(root: &mut Value, create: bool) -> anyhow::Result<Option<&mut Map<String, Value>>> {
    let Some(obj) = root.as_object_mut() else { bail!("settings.json is not a JSON object") };
    if !obj.contains_key("env") {
        if !create {
            return Ok(None);
        }
        obj.insert("env".into(), Value::Object(Map::new()));
    }
    match obj.get_mut("env") {
        Some(Value::Object(env)) => Ok(Some(env)),
        _ => bail!("`env` in settings.json is not an object"),
    }
}

/// Installs the mod and registers it. `Ok(false)`: not installed because the known Claude version
/// is older than [`MOD_MIN_CLAUDE_VERSION`] (an unknown version installs). `sich_config_dir` is
/// the `<base>/sich` directory.
pub fn install(
    claude_config_dir: &Path,
    sich_config_dir: &Path,
    claude_version: Option<&str>,
) -> anyhow::Result<bool> {
    let min = parse_version_triple(MOD_MIN_CLAUDE_VERSION).expect("valid constant");
    if claude_version.and_then(parse_version_triple).is_some_and(|version| version < min) {
        return Ok(false);
    }
    let _edit = SETTINGS_EDIT.lock().map_err(|_| anyhow::anyhow!("Claude settings edit lock poisoned"))?;
    let path = settings_path(claude_config_dir);
    // Read first: an unusable settings.json must leave everything untouched.
    let original = read_settings(&path)?;
    let mut root = original.clone().unwrap_or_else(|| Value::Object(Map::new()));
    let dir = mod_dir(sich_config_dir);
    let entry = entry_for(&dir);
    let env = env_object(&mut root, true)?.expect("created");
    let current = match env.get(ENV_KEY) {
        None => "",
        Some(Value::String(list)) => list.as_str(),
        Some(_) => bail!("`env.{ENV_KEY}` in settings.json is not a string"),
    };
    let next = add_entry(current, &entry, LIST_SEPARATOR, cfg!(windows));

    write_files(&dir)?;
    if let Some(next) = next {
        env.insert(ENV_KEY.into(), Value::String(next));
    }
    if original.as_ref() != Some(&root) {
        write_settings(claude_config_dir, &root, original.is_some())?;
    }
    Ok(true)
}

/// Removes our list entry (and the key when it becomes empty; `env` itself stays) and deletes the
/// mod folder.
pub fn uninstall(claude_config_dir: &Path, sich_config_dir: &Path) -> anyhow::Result<()> {
    let _edit = SETTINGS_EDIT.lock().map_err(|_| anyhow::anyhow!("Claude settings edit lock poisoned"))?;
    if let Some(original) = read_settings(&settings_path(claude_config_dir))? {
        let mut root = original.clone();
        if let Some(env) = env_object(&mut root, false)?
            && let Some(list) = env.get(ENV_KEY)
        {
            let Some(list) = list.as_str() else { bail!("`env.{ENV_KEY}` in settings.json is not a string") };
            if let Some(rest) = remove_entries(list, LIST_SEPARATOR, cfg!(windows)) {
                if rest.is_empty() {
                    env.shift_remove(ENV_KEY);
                } else {
                    env.insert(ENV_KEY.into(), Value::String(rest));
                }
            }
        }
        if root != original {
            write_settings(claude_config_dir, &root, true)?;
        }
    }
    match std::fs::remove_dir_all(mod_dir(sich_config_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("remove the Sich Claude mod folder"),
    }
}

/// True when the folder is listed in `settings.json` and its entry file exists.
pub fn is_installed(claude_config_dir: &Path, sich_config_dir: &Path) -> bool {
    let dir = mod_dir(sich_config_dir);
    if !dir.join("hooks").join("register.tsx").is_file() {
        return false;
    }
    let Ok(Some(root)) = read_settings(&settings_path(claude_config_dir)) else { return false };
    let want = key(&entry_for(&dir), cfg!(windows));
    root.get("env")
        .and_then(|env| env.get(ENV_KEY))
        .and_then(Value::as_str)
        .is_some_and(|list| list.split(LIST_SEPARATOR).any(|item| key(item, cfg!(windows)) == want))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const SEP: char = LIST_SEPARATOR;

    struct Dirs {
        _temp: tempfile::TempDir,
        claude: PathBuf,
        sich: PathBuf,
    }

    fn dirs() -> Dirs {
        let temp = tempfile::tempdir().unwrap();
        let claude = temp.path().join("claude");
        let sich = temp.path().join("sich");
        fs::create_dir_all(&claude).unwrap();
        Dirs { _temp: temp, claude, sich }
    }

    fn write(d: &Dirs, text: &str) {
        fs::write(d.claude.join("settings.json"), text).unwrap();
    }

    fn read(d: &Dirs) -> Value {
        serde_json::from_str(&fs::read_to_string(d.claude.join("settings.json")).unwrap()).unwrap()
    }

    fn list(d: &Dirs) -> String {
        read(d)["env"][ENV_KEY].as_str().unwrap().to_owned()
    }

    fn ours(d: &Dirs) -> String {
        entry_for(&mod_dir(&d.sich))
    }

    #[test]
    fn embedded_files_are_complete() {
        for (rel, content) in FILES {
            assert!(!content.trim().is_empty(), "{rel}");
        }
        let plugin: Value = serde_json::from_str(FILES[0].1).unwrap();
        assert_eq!(plugin["name"], "sich-context");
        assert!(FILES.iter().any(|(rel, _)| *rel == "hooks/register.tsx"));
        let hooks: Value = serde_json::from_str(FILES[1].1).unwrap();
        assert_eq!(hooks["modules"], json!(["./register.tsx"]));
    }

    #[test]
    fn adds_to_missing_env_and_missing_file() {
        let d = dirs();
        write(&d, r#"{"model":"opus"}"#);
        assert!(install(&d.claude, &d.sich, Some("2.1.330")).unwrap());
        let v = read(&d);
        assert_eq!(v["model"], "opus");
        assert_eq!(v["env"][ENV_KEY], json!(ours(&d)));
        assert!(!ours(&d).contains('\\'));
        assert!(is_installed(&d.claude, &d.sich));
        assert!(d.sich.join("claude-mod/sich-context/hooks/register.tsx").is_file());
        // The backup of the original exists and the temp file is gone.
        assert_eq!(fs::read_to_string(d.claude.join("settings.json.sich-backup")).unwrap(), r#"{"model":"opus"}"#);
        assert!(!d.claude.join("settings.json.sich-tmp").exists());

        let fresh = dirs();
        fs::remove_dir_all(&fresh.claude).unwrap();
        assert!(install(&fresh.claude, &fresh.sich, None).unwrap());
        assert_eq!(list(&fresh), ours(&fresh));
    }

    #[test]
    fn appends_to_an_existing_list_preserving_others_and_key_order() {
        let d = dirs();
        let others = format!("C:/mods/a{SEP}D:/mods/b");
        write(&d, &json!({"zeta": 1, "env": {"A": "1", ENV_KEY: others, "Z": "2"}, "alpha": [1]}).to_string());
        install(&d.claude, &d.sich, Some("2.1.330")).unwrap();
        let v = read(&d);
        assert_eq!(list(&d), format!("{others}{SEP}{}", ours(&d)));
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["zeta", "env", "alpha"]);
        let env_keys: Vec<&String> = v["env"].as_object().unwrap().keys().collect();
        assert_eq!(env_keys, ["A", ENV_KEY, "Z"]);
    }

    #[test]
    fn install_is_idempotent_and_does_not_rewrite_settings() {
        let d = dirs();
        write(&d, r#"{"env":{"X":"1"}}"#);
        install(&d.claude, &d.sich, None).unwrap();
        let first = fs::read_to_string(d.claude.join("settings.json")).unwrap();
        install(&d.claude, &d.sich, None).unwrap();
        assert_eq!(fs::read_to_string(d.claude.join("settings.json")).unwrap(), first);
        assert_eq!(list(&d).matches("sich-context").count(), 1);
    }

    #[test]
    fn list_helpers_normalize_and_dedupe() {
        let entry = "C:/cfg/sich/claude-mod/sich-context";
        // Already listed in another spelling: untouched on Windows rules.
        assert_eq!(add_entry(r"c:\CFG\sich\claude-mod\sich-context\", entry, ';', true), None);
        assert_eq!(add_entry(entry, entry, ';', false), None);
        // Case differs on a case-sensitive platform: stays ours by suffix, replaced in place.
        assert_eq!(
            add_entry("/a:/CFG/claude-mod/sich-context:/b", "/cfg/claude-mod/sich-context", ':', false).as_deref(),
            Some("/a:/cfg/claude-mod/sich-context:/b")
        );
        // A stale location moves in place and duplicates collapse.
        assert_eq!(
            add_entry("x;C:/old/claude-mod/sich-context;y;C:/old2/claude-mod/sich-context", entry, ';', true).as_deref(),
            Some(&format!("x;{entry};y")[..])
        );
        // Similar but different folders are not ours.
        assert_eq!(
            add_entry("C:/m/claude-mod/sich-context-2", entry, ';', true).as_deref(),
            Some(&format!("C:/m/claude-mod/sich-context-2;{entry}")[..])
        );
        assert_eq!(remove_entries("a;b", ';', true), None);
        assert_eq!(remove_entries(&format!("a;{entry};b"), ';', true).as_deref(), Some("a;b"));
        assert_eq!(remove_entries(entry, ';', true).as_deref(), Some(""));
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let d = dirs();
        let others = format!("C:/mods/a{SEP}D:/mods/b");
        write(&d, &json!({"env": {ENV_KEY: others, "K": "v"}}).to_string());
        install(&d.claude, &d.sich, None).unwrap();
        uninstall(&d.claude, &d.sich).unwrap();
        assert_eq!(read(&d), json!({"env": {ENV_KEY: others, "K": "v"}}));
        assert!(!d.sich.join("claude-mod/sich-context").exists());
        assert!(!is_installed(&d.claude, &d.sich));
    }

    #[test]
    fn uninstall_drops_an_emptied_key_but_never_env() {
        let d = dirs();
        write(&d, "{}");
        install(&d.claude, &d.sich, None).unwrap();
        uninstall(&d.claude, &d.sich).unwrap();
        assert_eq!(read(&d), json!({"env": {}}));
        // Nothing to remove: no write, no error.
        let before = fs::read_to_string(d.claude.join("settings.json")).unwrap();
        uninstall(&d.claude, &d.sich).unwrap();
        assert_eq!(fs::read_to_string(d.claude.join("settings.json")).unwrap(), before);
        // No settings file at all.
        let none = dirs();
        fs::remove_dir_all(&none.claude).unwrap();
        uninstall(&none.claude, &none.sich).unwrap();
        assert!(!none.claude.exists());
    }

    #[test]
    fn invalid_json_and_wrong_shapes_are_left_untouched() {
        for content in ["{ oops", "[1]", r#"{"env":[]}"#, r#"{"env":{"CLAUDE_CODE_PLUGIN_DIRS":5}}"#] {
            let d = dirs();
            write(&d, content);
            assert!(install(&d.claude, &d.sich, None).is_err(), "{content}");
            assert!(uninstall(&d.claude, &d.sich).is_err(), "{content}");
            assert_eq!(fs::read_to_string(d.claude.join("settings.json")).unwrap(), content);
            assert!(!d.claude.join("settings.json.sich-backup").exists());
            assert!(!d.sich.exists(), "no mod files for unusable settings: {content}");
        }
    }

    #[test]
    fn version_gate() {
        for version in ["2.1.299", "1.0.0", "2.0.999"] {
            let d = dirs();
            assert!(!install(&d.claude, &d.sich, Some(version)).unwrap(), "{version}");
            assert!(!d.claude.join("settings.json").exists());
            assert!(!d.sich.exists());
        }
        for version in [Some("2.1.300"), Some("2.2.0"), Some("3.0.0-beta.1"), None, Some("garbage")] {
            let d = dirs();
            assert!(install(&d.claude, &d.sich, version).unwrap(), "{version:?}");
        }
    }

    #[test]
    fn mod_files_are_rewritten_only_when_changed() {
        let d = dirs();
        install(&d.claude, &d.sich, None).unwrap();
        let register = mod_dir(&d.sich).join("hooks/register.tsx");
        let readme = mod_dir(&d.sich).join("README.md");
        let modified = |p: &Path| fs::metadata(p).unwrap().modified().unwrap();
        let (r1, m1) = (modified(&register), modified(&readme));
        std::thread::sleep(std::time::Duration::from_millis(30));
        install(&d.claude, &d.sich, None).unwrap();
        assert_eq!((modified(&register), modified(&readme)), (r1, m1));
        // A tampered file is restored; its sibling stays untouched.
        fs::write(&register, "// edited").unwrap();
        install(&d.claude, &d.sich, None).unwrap();
        assert_eq!(fs::read_to_string(&register).unwrap(), FILES[2].1);
        assert_eq!(modified(&readme), m1);
        // A generated types folder from the engine is left alone by install.
        let generated = mod_dir(&d.sich).join(".claude-plugin/types");
        fs::create_dir_all(&generated).unwrap();
        install(&d.claude, &d.sich, None).unwrap();
        assert!(generated.is_dir());
    }

    #[test]
    fn is_installed_needs_entry_and_files() {
        let d = dirs();
        assert!(!is_installed(&d.claude, &d.sich));
        install(&d.claude, &d.sich, None).unwrap();
        assert!(is_installed(&d.claude, &d.sich));
        fs::remove_dir_all(d.sich.join("claude-mod")).unwrap();
        assert!(!is_installed(&d.claude, &d.sich));
        write(&d, "{}");
        write_files(&mod_dir(&d.sich)).unwrap();
        assert!(!is_installed(&d.claude, &d.sich), "files without a list entry");
    }
}
