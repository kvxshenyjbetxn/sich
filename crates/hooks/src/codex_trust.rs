//! Approve only Sich's installed Codex hooks and track the hashes Sich owns for uninstall.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::NamedTempFile;

use crate::codex_app_server::{Client, bounded};
use crate::codex_install::{self, InstallReport, codex_command, events_for_version};

pub const LEDGER_FILE: &str = "codex-hook-trust.json";

type Hashes = BTreeMap<String, String>;
type Ledger = BTreeMap<PathBuf, Hashes>;

/// Called on a background thread after a successful hook install, including at startup.
pub fn approve_installed(
    codex_home: &Path,
    exe: &Path,
    report: &InstallReport,
    ledger: &Path,
) -> anyhow::Result<()> {
    let events = events_for_version(report.codex_version.as_deref());
    if report.skipped.is_some() || events.is_empty() {
        return Ok(());
    }
    let home = fs::canonicalize(codex_home).context("resolve Codex home")?;
    let command = codex_install::hook_command(exe);
    let hooks_path = home.join("hooks.json");
    let root = codex_install::read_hooks(&hooks_path)?.context("installed hooks.json is missing")?;
    let expected = events
        .iter()
        .map(|event| installed_slot(&root, event.name, event.timeout_secs, &command))
        .collect::<anyhow::Result<Vec<_>>>()?;
    bounded(approve(&home, &hooks_path, &command, &expected, ledger))
}

/// Called after the hook definitions have been removed. Probe Codex only if there are approvals
/// for this home to clean up. A failed or unsupported cleanup leaves the ledger for a later retry.
pub fn clear_approvals(codex_home: &Path, ledger: &Path) -> anyhow::Result<()> {
    if !ledger.try_exists()? || !codex_home.try_exists()? {
        return Ok(());
    }
    let home = fs::canonicalize(codex_home).context("resolve Codex home")?;
    let _lock = lock_ledger(ledger)?;
    let mut approvals = read_ledger(ledger)?;
    let Some(owned) = approvals.remove(&home) else { return Ok(()) };
    let version = codex_install::probe_codex_version_in(&home);
    ensure!(
        !events_for_version(version.as_deref()).is_empty(),
        "Codex 0.129 or newer is required to clear hook trust"
    );
    bounded(clear(&home, &owned))?;
    stage_ledger(ledger, &approvals)?.persist(ledger).context("update Sich hook trust ledger")?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct Slot {
    event: String,
    key_suffix: String,
}

fn installed_slot(root: &Value, event: &str, timeout: u64, command: &str) -> anyhow::Result<Slot> {
    let groups = root["hooks"][event].as_array().context("installed hook event is missing")?;
    let desired = json!({ "type": "command", "command": command, "timeout": timeout });
    let mut matches = groups
        .iter()
        .enumerate()
        .flat_map(|(group_index, group)| {
            group["hooks"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
                .map(move |(handler_index, hook)| (group_index, handler_index, hook))
        })
        .filter(|(_, _, hook)| **hook == desired);
    let (group, handler, _) = matches.next().with_context(|| format!("Sich hook {event} is missing"))?;
    ensure!(matches.next().is_none(), "ambiguous Sich hook {event}");
    let mut snake_event = String::new();
    for (i, c) in event.chars().enumerate() {
        if i > 0 && c.is_ascii_uppercase() {
            snake_event.push('_');
        }
        snake_event.push(c.to_ascii_lowercase());
    }
    let mut rpc_event = event.to_owned();
    rpc_event[..1].make_ascii_lowercase();
    Ok(Slot { event: rpc_event, key_suffix: format!("{snake_event}:{group}:{handler}") })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
enum TrustStatus {
    Trusted,
    Managed,
    Untrusted,
    Modified,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Hook {
    key: Option<String>,
    event_name: Option<String>,
    handler_type: Option<String>,
    command: Option<String>,
    source: Option<String>,
    source_path: Option<PathBuf>,
    current_hash: Option<String>,
    trust_status: Option<TrustStatus>,
}

#[derive(Deserialize)]
struct HookInventory {
    data: Vec<HookList>,
}

#[derive(Deserialize)]
struct HookList {
    hooks: Vec<Hook>,
}

fn our_hooks<'a>(
    hooks: &'a [Hook],
    path: &Path,
    command: &str,
    expected: &[Slot],
) -> anyhow::Result<Vec<&'a Hook>> {
    let ours = hooks
        .iter()
        .filter(|hook| {
            hook.command.as_deref() == Some(command)
                && hook.handler_type.as_deref() == Some("command")
                && hook.source.as_deref() == Some("user")
                && hook.source_path.as_deref().is_some_and(|source| same_path(source, path))
        })
        .collect::<Vec<_>>();
    expected
        .iter()
        .map(|slot| {
            let mut matches = ours.iter().copied().filter(|hook| {
                let key_parts = hook.key.as_deref().and_then(|key| {
                    key.strip_suffix(&slot.key_suffix).and_then(|prefix| prefix.strip_suffix(':'))
                });
                hook.event_name.as_deref() == Some(slot.event.as_str())
                    && key_parts.is_some_and(|source| same_path(Path::new(source), path))
            });
            let hook =
                matches.next().with_context(|| format!("Codex did not list Sich hook {}", slot.event))?;
            ensure!(matches.next().is_none(), "Codex listed duplicate Sich hook {}", slot.event);
            ensure!(
                hook.current_hash.as_deref().is_some_and(|hash| !hash.is_empty())
                    && matches!(hook.trust_status, Some(status) if status != TrustStatus::Unknown),
                "Codex did not return trust information for Sich hook {}",
                slot.event
            );
            Ok(hook)
        })
        .collect()
}

fn pending_approvals(hooks: &[&Hook]) -> Hashes {
    hooks
        .iter()
        .filter(|hook| matches!(hook.trust_status, Some(TrustStatus::Untrusted | TrustStatus::Modified)))
        .filter_map(|hook| Some((hook.key.clone()?, hook.current_hash.clone()?)))
        .collect()
}

fn same_path(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        fn normalize(path: &Path) -> String {
            let path = path.to_string_lossy().replace('/', "\\");
            if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
                return format!(r"\\{rest}");
            }
            path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()
        }
        normalize(a).eq_ignore_ascii_case(&normalize(b))
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn list_params(cwd: &str) -> Value {
    json!({ "cwds": [cwd] })
}

fn grant_edit(hashes: &Hashes) -> Value {
    let state: BTreeMap<_, _> =
        hashes.iter().map(|(key, hash)| (key, json!({ "trusted_hash": hash }))).collect();
    json!({ "keyPath": "hooks.state", "value": state, "mergeStrategy": "upsert" })
}

fn clear_edits(hashes: &Hashes) -> Vec<Value> {
    hashes
        .keys()
        .map(|key| {
            let key = json!(key); // JSON string quoting escapes dots and backslashes for the TOML path.
            json!({
                "keyPath": format!("hooks.state.{key}.trusted_hash"),
                "value": null,
                "mergeStrategy": "replace"
            })
        })
        .collect()
}

fn clear_state_edit(layer: &ConfigLayer, matching: &Hashes) -> anyhow::Result<Value> {
    let mut state = layer.config["hooks"]["state"].as_object().context("invalid Codex hook state")?.clone();
    for key in matching.keys() {
        let entry =
            state.get_mut(key).and_then(Value::as_object_mut).context("invalid Codex hook state entry")?;
        entry.remove("trusted_hash");
    }
    Ok(json!({ "keyPath": "hooks.state", "value": state, "mergeStrategy": "replace" }))
}

fn batch_params(edits: Vec<Value>, version: &str) -> Value {
    json!({ "edits": edits, "expectedVersion": version, "reloadUserConfig": true })
}

#[derive(Deserialize)]
struct ConfigRead {
    layers: Vec<ConfigLayer>,
}

#[derive(Deserialize)]
struct ConfigLayer {
    name: ConfigSource,
    version: String,
    config: Value,
}

#[derive(Deserialize)]
struct ConfigSource {
    #[serde(rename = "type")]
    kind: String,
    file: Option<PathBuf>,
}

impl ConfigLayer {
    fn trusted_hash(&self, key: &str) -> Option<&str> {
        self.config["hooks"]["state"][key]["trusted_hash"].as_str()
    }
}

fn matching_hashes(owned: &Hashes, layer: &ConfigLayer) -> Hashes {
    owned
        .iter()
        .filter(|(key, hash)| layer.trusted_hash(key) == Some(hash.as_str()))
        .map(|(key, hash)| (key.clone(), hash.clone()))
        .collect()
}

fn user_layer(response: ConfigRead, path: &Path) -> anyhow::Result<ConfigLayer> {
    response
        .layers
        .into_iter()
        .find(|layer| {
            layer.name.kind == "user"
                && layer.name.file.as_deref().is_some_and(|file| same_path(file, path))
                && !layer.version.is_empty()
                && layer.config.is_object()
        })
        .context("Codex user config layer unavailable")
}

async fn read_config(client: &mut Client, cwd: &str, path: &Path) -> anyhow::Result<ConfigLayer> {
    let response = client.call("config/read", json!({ "cwd": cwd, "includeLayers": true })).await?;
    user_layer(response, path)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigWrite {
    status: String,
    file_path: PathBuf,
    version: String,
}

async fn write_config(
    client: &mut Client,
    path: &Path,
    edits: Vec<Value>,
    version: &str,
) -> anyhow::Result<String> {
    let response: ConfigWrite = client.call("config/batchWrite", batch_params(edits, version)).await?;
    ensure!(
        matches!(response.status.as_str(), "ok" | "okOverridden") && same_path(&response.file_path, path),
        "Codex did not confirm the user config write"
    );
    Ok(response.version)
}

async fn list_hooks(client: &mut Client, cwd: &str) -> anyhow::Result<Vec<Hook>> {
    let response: HookInventory = client.call("hooks/list", list_params(cwd)).await?;
    let [list]: [HookList; 1] = response.data.try_into().ok().context("expected one Codex hook inventory")?;
    Ok(list.hooks)
}

async fn approve(
    home: &Path,
    hooks_path: &Path,
    command: &str,
    expected: &[Slot],
    ledger: &Path,
) -> anyhow::Result<()> {
    let cwd = home.to_str().context("Codex home is not a UTF-8 path")?;
    let mut client = Client::connect(codex_command("app-server"), home).await?;
    let hooks = list_hooks(&mut client, cwd).await?;
    let mut pending = pending_approvals(&our_hooks(&hooks, hooks_path, command, expected)?);
    if pending.is_empty() {
        return Ok(());
    }

    let _lock = lock_ledger(ledger)?;
    let config_path = home.join("config.toml");
    let layer = read_config(&mut client, cwd, &config_path).await?;
    // The user may have approved a hook between list and read. Only record hashes we write.
    pending.retain(|key, hash| layer.trusted_hash(key) != Some(hash));
    if !pending.is_empty() {
        let mut approvals = read_ledger(ledger)?;
        approvals.entry(home.to_owned()).or_default().extend(pending.clone());
        let staged = stage_ledger(ledger, &approvals)?;
        backup_config(&config_path)?;
        write_config(&mut client, &config_path, vec![grant_edit(&pending)], &layer.version).await?;
        // Record a successful write even if the subsequent inventory check fails.
        staged.persist(ledger).context("record Sich hook approvals")?;
    }

    let hooks = list_hooks(&mut client, cwd).await?;
    let ours = our_hooks(&hooks, hooks_path, command, expected)?;
    ensure!(pending_approvals(&ours).is_empty(), "Codex still reports Sich hooks awaiting review");
    ensure!(
        pending.iter().all(|(key, hash)| ours.iter().any(|hook| {
            hook.key.as_ref() == Some(key)
                && hook.current_hash.as_ref() == Some(hash)
                && hook.trust_status == Some(TrustStatus::Trusted)
        })),
        "Codex did not verify the written Sich hook hashes"
    );
    Ok(())
}

async fn clear(home: &Path, owned: &Hashes) -> anyhow::Result<()> {
    let cwd = home.to_str().context("Codex home is not a UTF-8 path")?;
    let mut client = Client::connect(codex_command("app-server"), home).await?;
    let config_path = home.join("config.toml");
    let layer = read_config(&mut client, cwd, &config_path).await?;
    let matching = matching_hashes(owned, &layer);
    if !matching.is_empty() {
        backup_config(&config_path)?;
        let written = write_config(&mut client, &config_path, clear_edits(&matching), &layer.version).await?;
        let mut updated = read_config(&mut client, cwd, &config_path).await?;
        if written == layer.version
            && updated.version == layer.version
            && !matching_hashes(&matching, &updated).is_empty()
        {
            // Older app-servers split key paths on every dot, even inside quoted keys.
            // They return success for the no-op clear. Keep every other value in hooks.state.
            let edit = clear_state_edit(&updated, &matching)?;
            write_config(&mut client, &config_path, vec![edit], &updated.version).await?;
            updated = read_config(&mut client, cwd, &config_path).await?;
        }
        ensure!(matching_hashes(&matching, &updated).is_empty(), "Codex still holds Sich hook approvals");
    }
    Ok(())
}

fn lock_ledger(path: &Path) -> anyhow::Result<File> {
    let dir = path.parent().context("Sich hook trust ledger has no parent")?;
    fs::create_dir_all(dir).context("create Sich config directory")?;
    let lock = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .context("open Sich hook trust lock")?;
    lock.try_lock().context("lock Sich hook trust ledger")?;
    Ok(lock)
}

fn read_ledger(path: &Path) -> anyhow::Result<Ledger> {
    match File::open(path) {
        Ok(file) => serde_json::from_reader(file).context("parse Sich hook trust ledger"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::new()),
        Err(err) => Err(err).context("read Sich hook trust ledger"),
    }
}

fn stage_ledger(path: &Path, ledger: &Ledger) -> anyhow::Result<NamedTempFile> {
    let dir = path.parent().context("Sich hook trust ledger has no parent")?;
    let mut temp = NamedTempFile::new_in(dir).context("stage Sich hook trust ledger")?;
    serde_json::to_writer_pretty(&mut temp, ledger).context("serialize Sich hook trust ledger")?;
    temp.as_file().sync_all().context("flush Sich hook trust ledger")?;
    Ok(temp)
}

fn backup_config(path: &Path) -> anyhow::Result<()> {
    let backup = path.with_file_name("config.toml.sich-backup");
    if backup.try_exists()? {
        return Ok(());
    }
    let mut temp = NamedTempFile::new_in(path.parent().context("Codex config has no parent")?)?;
    match File::open(path) {
        Ok(mut config) => {
            std::io::copy(&mut config, &mut temp).context("copy Codex config backup")?;
        }
        // An absent config is equivalent to an empty one. Keep that first snapshot too.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("read Codex config for backup"),
    }
    temp.as_file().sync_all().context("flush Codex config backup")?;
    match temp.persist_noclobber(backup) {
        Ok(_) => Ok(()),
        Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err).context("save Codex config backup"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = r#""C:/Sich App/sich.exe" hook codex"#;
    const SOURCE: &str = r"C:\Users\Test\.codex\hooks.json";

    fn slot(root: &Value, event: &str) -> Slot {
        installed_slot(root, event, 5, COMMAND).unwrap()
    }

    fn fixture_hook(event: &str, suffix: &str, status: &str) -> Value {
        json!({
            "key": format!("{SOURCE}:{suffix}"),
            "eventName": event,
            "handlerType": "command",
            "command": COMMAND,
            "sourcePath": SOURCE,
            "source": "user",
            "currentHash": format!("sha256:{event}"),
            "trustStatus": status,
            "enabled": false,
            "displayOrder": 3,
            "timeoutSec": 5,
            "matcher": null,
            "isManaged": false,
            "additionalContextLimit": null,
            "environmentId": null,
            "futureField": true
        })
    }

    #[test]
    fn request_shapes_keep_exact_keys_and_only_write_the_hash() {
        let key = r"C:\Users\Test\.codex\hooks.json:pre_tool_use:1:0";
        let hashes = Hashes::from([(key.into(), "sha256:example".into())]);
        assert_eq!(list_params(r"C:\Users\Test"), json!({ "cwds": [r"C:\Users\Test"] }));
        assert_eq!(
            batch_params(vec![grant_edit(&hashes)], "v1"),
            json!({
                "edits": [{
                    "keyPath": "hooks.state",
                    "value": { (key): { "trusted_hash": "sha256:example" } },
                    "mergeStrategy": "upsert"
                }],
                "expectedVersion": "v1",
                "reloadUserConfig": true
            })
        );
        assert_eq!(
            clear_edits(&hashes),
            vec![json!({
                "keyPath": r#"hooks.state."C:\\Users\\Test\\.codex\\hooks.json:pre_tool_use:1:0".trusted_hash"#,
                "value": null,
                "mergeStrategy": "replace"
            })]
        );
    }

    #[test]
    fn selection_checks_file_command_positions_and_trust_status() {
        let desired = json!({ "type": "command", "command": COMMAND, "timeout": 5 });
        let group = json!({ "hooks": [desired] });
        let root = json!({ "hooks": {
            "PreToolUse": [{ "hooks": [{ "command": "user" }] }, group],
            "Stop": [group],
            "PermissionRequest": [group]
        } });
        let slots = [slot(&root, "PreToolUse"), slot(&root, "Stop"), slot(&root, "PermissionRequest")];
        let pre = fixture_hook("preToolUse", "pre_tool_use:1:0", "untrusted");
        let mut other = pre.clone();
        other["command"] = json!("orca hook codex");
        let mut other_source = pre.clone();
        other_source["sourcePath"] = json!(r"C:\other\hooks.json");
        let mut other_position = pre.clone();
        other_position["key"] = json!(format!("{SOURCE}:pre_tool_use:0:0"));
        let mut other_layer = pre.clone();
        other_layer["source"] = json!("project");
        let mut other_handler = pre.clone();
        other_handler["handlerType"] = json!("prompt");
        let mut other_event = pre.clone();
        other_event["eventName"] = json!("sessionStart");
        let mut no_optionals = fixture_hook("permissionRequest", "permission_request:0:0", "modified");
        no_optionals.as_object_mut().unwrap().remove("environmentId");
        no_optionals.as_object_mut().unwrap().remove("matcher");
        let reply: HookInventory = serde_json::from_value(json!({ "data": [{
            "cwd": r"C:\Users\Test",
            "hooks": [
                other, other_source, other_position, other_layer, other_handler, other_event,
                { "handlerType": "agent", "command": null },
                pre, fixture_hook("stop", "stop:0:0", "trusted"), no_optionals
            ],
            "warnings": [],
            "errors": []
        }] }))
        .unwrap();
        let ours = our_hooks(&reply.data[0].hooks, Path::new(SOURCE), COMMAND, &slots).unwrap();
        assert_eq!(ours.len(), 3);
        assert_eq!(
            pending_approvals(&ours),
            Hashes::from([
                (format!("{SOURCE}:pre_tool_use:1:0"), "sha256:preToolUse".into()),
                (format!("{SOURCE}:permission_request:0:0"), "sha256:permissionRequest".into()),
            ])
        );
    }

    #[test]
    fn selection_requires_unambiguous_coverage_and_trust_fields() {
        let path = Path::new(SOURCE);
        let expected = [Slot { event: "stop".into(), key_suffix: "stop:0:0".into() }];
        let hook = fixture_hook("stop", "stop:0:0", "modified");
        assert!(our_hooks(&[], path, COMMAND, &expected).is_err());
        let duplicate = serde_json::from_value::<Vec<Hook>>(json!([hook, hook])).unwrap();
        assert!(our_hooks(&duplicate, path, COMMAND, &expected).is_err());
        for field in ["trustStatus", "currentHash", "key"] {
            let mut missing = hook.clone();
            missing.as_object_mut().unwrap().remove(field);
            let hooks = [serde_json::from_value(missing).unwrap()];
            assert!(our_hooks(&hooks, path, COMMAND, &expected).is_err());
        }
    }

    #[test]
    fn versions_below_0129_skip_before_any_io() {
        let dir = tempfile::tempdir().unwrap();
        let nonexistent = dir.path().join("nonexistent");
        let ledger = dir.path().join(LEDGER_FILE);
        for version in [None, Some("invalid"), Some("0.128.0")] {
            let report = InstallReport { codex_version: version.map(str::to_owned), ..Default::default() };
            approve_installed(&nonexistent, Path::new("sich"), &report, &ledger).unwrap();
        }
        let supported = InstallReport { codex_version: Some("0.129.0".into()), ..Default::default() };
        assert!(approve_installed(&nonexistent, Path::new("sich"), &supported, &ledger).is_err());
        assert!(!ledger.exists());
    }

    #[test]
    fn ledger_roundtrips_and_cleanup_uses_only_matching_hashes_in_the_user_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LEDGER_FILE);
        let _lock = lock_ledger(&path).unwrap();
        assert!(lock_ledger(&path).is_err());
        assert!(read_ledger(&path).unwrap().is_empty());
        let owned = Hashes::from([
            ("matching".into(), "sha256:a".into()),
            ("changed".into(), "sha256:b".into()),
            ("gone".into(), "sha256:c".into()),
        ]);
        let ledger = Ledger::from([(dir.path().to_owned(), owned.clone())]);
        stage_ledger(&path, &ledger).unwrap().persist(&path).unwrap();
        let reloaded = read_ledger(&path).unwrap();
        assert_eq!(reloaded, ledger);

        let config_path = dir.path().join("config.toml");
        let response = serde_json::from_value(json!({ "layers": [
            {
                "name": { "type": "project" },
                "version": "project-version",
                "config": { "hooks": { "state": { "gone": { "trusted_hash": "sha256:c" } } } }
            },
            {
                "name": { "type": "user", "file": config_path },
                "version": "user-version",
                "config": { "hooks": { "state": {
                    "matching": { "trusted_hash": "sha256:a", "enabled": false },
                    "changed": { "trusted_hash": "sha256:d" },
                    "user": { "trusted_hash": "sha256:e" }
                } } }
            }
        ] }))
        .unwrap();
        let layer = user_layer(response, &config_path).unwrap();
        assert_eq!(layer.version, "user-version");
        let matching = matching_hashes(&reloaded[dir.path()], &layer);
        assert_eq!(matching, Hashes::from([("matching".into(), "sha256:a".into())]));
        assert_eq!(
            clear_edits(&matching),
            vec![json!({
                "keyPath": "hooks.state.\"matching\".trusted_hash", "value": null, "mergeStrategy": "replace"
            })]
        );

        assert_eq!(
            clear_state_edit(&layer, &matching).unwrap(),
            json!({
                "keyPath": "hooks.state", "mergeStrategy": "replace",
                "value": {
                    "matching": { "enabled": false },
                    "changed": { "trusted_hash": "sha256:d" },
                    "user": { "trusted_hash": "sha256:e" }
                }
            })
        );

        fs::write(&path, "{bad").unwrap();
        assert!(read_ledger(&path).is_err());
    }

    #[test]
    fn config_backup_is_written_once_and_represents_an_absent_file() {
        for initial in [None, Some("model = \"original\"\n")] {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config.toml");
            let backup = dir.path().join("config.toml.sich-backup");
            if let Some(initial) = initial {
                fs::write(&config, initial).unwrap();
            }
            backup_config(&config).unwrap();
            fs::write(&config, "model = \"changed\"\n").unwrap();
            backup_config(&config).unwrap();
            assert_eq!(fs::read_to_string(backup).unwrap(), initial.unwrap_or_default());
            assert_eq!(fs::read_to_string(config).unwrap(), "model = \"changed\"\n");
        }
    }

    #[test]
    #[ignore = "requires a real Codex >= 0.129 on PATH"]
    fn real_app_server_grants_idempotently_and_clears_only_owned_hashes() {
        let codex_home = tempfile::tempdir().unwrap();
        let sich_config = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(codex_home.path()).unwrap();
        let ledger = sich_config.path().join(LEDGER_FILE);
        let version = codex_install::probe_codex_version_in(&home).unwrap();
        assert!(!events_for_version(Some(&version)).is_empty());
        let exe = home.join("sich.exe");
        let report = codex_install::install(&home, &exe, Some(&version)).unwrap();

        // Seed disabled enablement for one of Sich's slots and a foreign approval.
        let cwd = home.to_str().unwrap();
        let config = home.join("config.toml");
        let key =
            bounded(async {
                let mut client = Client::connect(codex_command("app-server"), &home).await?;
                let hooks = list_hooks(&mut client, cwd).await?;
                let command = codex_install::hook_command(&exe);
                let key = hooks
                    .iter()
                    .find(|h| h.command.as_deref() == Some(&command))
                    .and_then(|h| h.key.as_deref())
                    .context("missing installed hook")?;
                let layer = read_config(&mut client, cwd, &config).await?;
                write_config(&mut client, &config, vec![json!({
                "keyPath": "hooks.state", "mergeStrategy": "upsert",
                "value": { (key): { "enabled": false }, "foreign": { "trusted_hash": "sha256:foreign" } }
            })], &layer.version).await?;
                Ok(key.to_owned())
            })
            .unwrap();
        let base_config = fs::read(&config).unwrap();

        approve_installed(&home, &exe, &report, &ledger).unwrap();
        let owned = read_ledger(&ledger).unwrap();
        assert_eq!(owned[&home].len(), report.installed.len());
        assert_eq!(fs::read(home.join("config.toml.sich-backup")).unwrap(), base_config);
        let installed_config = fs::read(&config).unwrap();
        approve_installed(&home, &exe, &report, &ledger).unwrap();
        assert_eq!(fs::read(&config).unwrap(), installed_config);

        // A later writer's changed value should survive uninstall even at a ledger-owned key.
        let changed_key = owned[&home].keys().find(|candidate| **candidate != key).unwrap();
        bounded(async {
            let mut client = Client::connect(codex_command("app-server"), &home).await?;
            let layer = read_config(&mut client, cwd, &config).await?;
            assert_eq!(layer.config["hooks"]["state"][&key]["enabled"], false);
            write_config(
                &mut client,
                &config,
                vec![grant_edit(&Hashes::from([(changed_key.clone(), "sha256:later".into())]))],
                &layer.version,
            )
            .await
        })
        .unwrap();

        codex_install::uninstall(&home, &exe).unwrap();
        clear_approvals(&home, &ledger).unwrap();
        assert!(read_ledger(&ledger).unwrap().is_empty());
        bounded(async {
            let mut client = Client::connect(codex_command("app-server"), &home).await?;
            let layer = read_config(&mut client, cwd, &config).await?;
            assert!(matching_hashes(&owned[&home], &layer).is_empty());
            assert_eq!(layer.config["hooks"]["state"][&key]["enabled"], false);
            assert_eq!(layer.trusted_hash("foreign"), Some("sha256:foreign"));
            assert_eq!(layer.trusted_hash(changed_key), Some("sha256:later"));
            Ok(())
        })
        .unwrap();
    }
}
