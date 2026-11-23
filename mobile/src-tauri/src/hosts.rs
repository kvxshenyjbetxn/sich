//! Paired PCs, persisted as `hosts.json` in the app data dir.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// Same as `endpoint_id`; stable key used by the UI.
    pub id: String,
    pub name: String,
    pub endpoint_id: String,
    /// Direct `ip:port` addresses of the gateway from the pairing link.
    pub addrs: Vec<String>,
    pub relay: Option<String>,
    pub paired_unix_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileData {
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    hosts: Vec<Host>,
}

#[derive(Debug)]
pub struct HostStore {
    path: PathBuf,
    data: FileData,
}

impl HostStore {
    /// Load the store. A missing file is an empty store. An unreadable/corrupt file is moved
    /// aside to `hosts.json.bad` (never silently overwritten) and the store starts empty.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let data = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<FileData>(&bytes) {
                Ok(d) => d,
                Err(_) => {
                    let mut bad = path.clone().into_os_string();
                    bad.push(".bad");
                    let _ = std::fs::rename(&path, PathBuf::from(bad));
                    FileData::default()
                }
            },
            Err(_) => FileData::default(),
        };
        Self { path, data }
    }

    pub fn hosts(&self) -> &[Host] {
        &self.data.hosts
    }

    pub fn active_id(&self) -> Option<&str> {
        self.data.active.as_deref()
    }

    pub fn get(&self, id: &str) -> Option<&Host> {
        self.data.hosts.iter().find(|h| h.id == id)
    }

    pub fn active(&self) -> Option<&Host> {
        self.active_id().and_then(|id| self.get(id))
    }

    /// Insert or replace (by id) and save.
    pub fn upsert(&mut self, host: Host) -> io::Result<()> {
        match self.data.hosts.iter_mut().find(|h| h.id == host.id) {
            Some(slot) => *slot = host,
            None => self.data.hosts.push(host),
        }
        self.save()
    }

    /// Remove a host (and clear `active` if it was active). Returns whether it existed.
    pub fn remove(&mut self, id: &str) -> io::Result<bool> {
        let before = self.data.hosts.len();
        self.data.hosts.retain(|h| h.id != id);
        let existed = self.data.hosts.len() != before;
        if self.data.active.as_deref() == Some(id) {
            self.data.active = None;
        }
        if existed {
            self.save()?;
        }
        Ok(existed)
    }

    /// Mark a known host as active. Returns false for an unknown id.
    pub fn set_active(&mut self, id: &str) -> io::Result<bool> {
        if self.get(id).is_none() {
            return Ok(false);
        }
        self.data.active = Some(id.to_string());
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> io::Result<()> {
        write_atomic(&self.path, &serde_json::to_vec_pretty(&self.data).map_err(io::Error::other)?)
    }
}

/// Write via a temp file + rename so a crash never leaves a half-written file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(id: &str) -> Host {
        Host {
            id: id.into(),
            name: format!("PC {id}"),
            endpoint_id: id.into(),
            addrs: vec!["192.168.0.2:1234".into()],
            relay: Some("https://relay.example/".into()),
            paired_unix_ms: 42,
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = HostStore::load(dir.path().join("hosts.json"));
        assert!(s.hosts().is_empty());
        assert!(s.active().is_none());
    }

    #[test]
    fn upsert_persists_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("hosts.json");
        let mut s = HostStore::load(&p);
        s.upsert(host("a")).unwrap();
        s.upsert(host("b")).unwrap();
        let mut renamed = host("a");
        renamed.name = "New".into();
        s.upsert(renamed.clone()).unwrap();
        let s2 = HostStore::load(&p);
        assert_eq!(s2.hosts().len(), 2);
        assert_eq!(s2.get("a"), Some(&renamed));
    }

    #[test]
    fn active_roundtrip_and_unknown_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hosts.json");
        let mut s = HostStore::load(&p);
        s.upsert(host("a")).unwrap();
        assert!(!s.set_active("zzz").unwrap());
        assert!(s.set_active("a").unwrap());
        assert_eq!(HostStore::load(&p).active().map(|h| h.id.as_str()), Some("a"));
    }

    #[test]
    fn remove_clears_active() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hosts.json");
        let mut s = HostStore::load(&p);
        s.upsert(host("a")).unwrap();
        s.set_active("a").unwrap();
        assert!(s.remove("a").unwrap());
        assert!(!s.remove("a").unwrap());
        let s2 = HostStore::load(&p);
        assert!(s2.hosts().is_empty());
        assert!(s2.active_id().is_none());
    }

    #[test]
    fn corrupt_file_is_moved_aside_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hosts.json");
        std::fs::write(&p, b"{ not json").unwrap();
        let mut s = HostStore::load(&p);
        assert!(s.hosts().is_empty());
        assert_eq!(std::fs::read(dir.path().join("hosts.json.bad")).unwrap(), b"{ not json");
        s.upsert(host("a")).unwrap();
        assert_eq!(HostStore::load(&p).hosts().len(), 1);
    }

    #[test]
    fn tolerates_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hosts.json");
        std::fs::write(&p, b"{}").unwrap();
        assert!(HostStore::load(&p).hosts().is_empty());
    }
}
