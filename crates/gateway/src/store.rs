//! Persistent gateway state: the iroh secret key and the list of paired devices.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

const KEY_FILE: &str = "gateway.key";
const DEVICES_FILE: &str = "devices.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub endpoint_id: String,
    pub name: String,
    pub paired_unix_ms: u64,
    pub last_seen_unix_ms: u64,
}

/// Paired devices. When `devices.json` exists but cannot be parsed the store is *poisoned*:
/// nobody is considered paired, and the file is never overwritten.
pub struct Store {
    dir: PathBuf,
    devices: Vec<Device>,
    poisoned: Option<String>,
}

fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.write_all(bytes)?;
    temp.flush()?;
    temp.persist(dir.join(name)).map_err(|e| e.error)?;
    Ok(())
}

impl Store {
    pub fn open(dir: &Path) -> Store {
        let path = dir.join(DEVICES_FILE);
        let (devices, poisoned) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<Device>>(&bytes) {
                Ok(devices) => (devices, None),
                Err(e) => (Vec::new(), Some(format!("{} is corrupt: {e}", path.display()))),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
            Err(e) => (Vec::new(), Some(format!("cannot read {}: {e}", path.display()))),
        };
        if let Some(reason) = &poisoned {
            tracing::error!("device list unusable, refusing all phones: {reason}");
        }
        Store { dir: dir.to_path_buf(), devices, poisoned }
    }

    pub fn poisoned(&self) -> Option<&str> {
        self.poisoned.as_deref()
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn is_paired(&self, endpoint_id: &str) -> bool {
        self.poisoned.is_none() && self.devices.iter().any(|d| d.endpoint_id == endpoint_id)
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(reason) = &self.poisoned {
            anyhow::bail!("device list is unusable: {reason}");
        }
        write_atomic(&self.dir, DEVICES_FILE, &serde_json::to_vec_pretty(&self.devices)?)
    }

    /// Adds a device, replacing an entry with the same endpoint id.
    pub fn add(&mut self, device: Device) -> anyhow::Result<()> {
        if let Some(reason) = &self.poisoned {
            anyhow::bail!("device list is unusable: {reason}");
        }
        let before = self.devices.clone();
        self.devices.retain(|d| d.endpoint_id != device.endpoint_id);
        self.devices.push(device);
        self.save().inspect_err(|_| self.devices = before)
    }

    /// Removes a device; `Ok(false)` when it was not paired.
    pub fn remove(&mut self, endpoint_id: &str) -> anyhow::Result<bool> {
        if let Some(reason) = &self.poisoned {
            anyhow::bail!("device list is unusable: {reason}");
        }
        let before = self.devices.clone();
        self.devices.retain(|d| d.endpoint_id != endpoint_id);
        if self.devices.len() == before.len() {
            return Ok(false);
        }
        self.save().inspect_err(|_| self.devices = before)?;
        Ok(true)
    }

    pub fn touch(&mut self, endpoint_id: &str, now_ms: u64) {
        if let Some(d) = self.devices.iter_mut().find(|d| d.endpoint_id == endpoint_id) {
            d.last_seen_unix_ms = now_ms;
            if let Err(e) = self.save() {
                tracing::warn!("could not save last-seen time: {e:#}");
            }
        }
    }
}

/// Loads the 32-byte iroh secret key, creating it once. A key file of the wrong size is an error
/// (never overwritten: that would silently change the gateway identity).
pub fn load_or_create_key(dir: &Path) -> anyhow::Result<[u8; 32]> {
    let path = dir.join(KEY_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| anyhow::anyhow!("{} has the wrong size ({} bytes)", path.display(), bytes.len())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = iroh::SecretKey::generate().to_bytes();
            write_atomic(dir, KEY_FILE, &key)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
            Ok(key)
        }
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str) -> Device {
        Device { endpoint_id: id.into(), name: format!("phone {id}"), paired_unix_ms: 1, last_seen_unix_ms: 2 }
    }

    #[test]
    fn missing_file_is_empty_and_usable() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path());
        assert!(store.poisoned().is_none());
        assert!(store.devices().is_empty());
        assert!(!store.is_paired("a"));
        store.add(dev("a")).unwrap();
        assert!(store.is_paired("a"));
    }

    #[test]
    fn devices_persist_and_replace_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path());
        store.add(dev("a")).unwrap();
        store.add(dev("b")).unwrap();
        store.add(Device { name: "renamed".into(), ..dev("a") }).unwrap();
        let again = Store::open(dir.path());
        assert_eq!(again.devices().len(), 2);
        assert_eq!(again.devices().iter().find(|d| d.endpoint_id == "a").unwrap().name, "renamed");
        // No temp files are left behind by the atomic write.
        let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(files.len(), 1, "{files:?}");
    }

    #[test]
    fn remove_reports_whether_it_existed() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path());
        store.add(dev("a")).unwrap();
        assert!(store.remove("a").unwrap());
        assert!(!store.remove("a").unwrap());
        assert!(Store::open(dir.path()).devices().is_empty());
    }

    #[test]
    fn corrupt_file_refuses_everyone_and_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DEVICES_FILE), b"{not json").unwrap();
        let mut store = Store::open(dir.path());
        assert!(store.poisoned().is_some());
        assert!(!store.is_paired("a"));
        assert!(store.add(dev("a")).is_err());
        assert!(store.remove("a").is_err());
        store.touch("a", 5);
        assert_eq!(std::fs::read(dir.path().join(DEVICES_FILE)).unwrap(), b"{not json");
    }

    #[test]
    fn touch_updates_last_seen() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(dir.path());
        store.add(dev("a")).unwrap();
        store.touch("a", 99);
        assert_eq!(Store::open(dir.path()).devices()[0].last_seen_unix_ms, 99);
    }

    #[test]
    fn key_is_created_once_and_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_key(dir.path()).unwrap();
        assert_eq!(load_or_create_key(dir.path()).unwrap(), first);
    }

    #[test]
    fn wrong_sized_key_is_an_error_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(KEY_FILE), b"short").unwrap();
        assert!(load_or_create_key(dir.path()).is_err());
        assert_eq!(std::fs::read(dir.path().join(KEY_FILE)).unwrap(), b"short");
    }
}
